//! LLM endpoints: the built-in provider endpoints plus any `[endpoints.<name>]`
//! from `tui.toml` (e.g. a LiteLLM proxy), and `endpoint:model` references.
//!
//! An [`Endpoint`] is a named place to send requests in one of Meerkat's
//! client [`WireDialect`]s. The built-in `anthropic`, `openai` and `gemini`
//! endpoints exist when the realm config or the environment has a key for
//! them; configured endpoints come from the layered `tui.toml`.
//!
//! # How an endpoint reaches the engine
//!
//! Sessions are built and hot-swapped by the factory, which resolves every
//! LLM client from the realm config. The TUI therefore projects its
//! endpoints into the realm's own config document, through the realm's
//! config runtime ([`meerkat_core::ConfigRuntime`]: the writer lock and
//! generation check every surface writes under; see [`project_endpoints`]):
//!
//! - **Built-in endpoints** (`anthropic`, `openai`, `gemini`) add nothing:
//!   sessions on them resolve their credentials exactly as `rkat` does (the
//!   realm's binding for the provider, else the environment default), and a
//!   switch back to one clears any explicit auth binding.
//! - **Configured endpoints of every dialect** (a LiteLLM or vLLM proxy in the
//!   OpenAI dialect, an Anthropic- or Gemini-dialect gateway) become one
//!   connection each in the realm's `config.toml`: a backend profile with
//!   the endpoint's `base_url`, an auth profile reading the environment
//!   variable `api_key_env` names (the document records the variable's
//!   name, never a key), and a binding. Keys come from the environment
//!   only: an inline `api_key` is refused at start-up
//!   ([`EndpointError::InlineApiKey`]). A session on such an endpoint carries
//!   the explicit [`AuthBindingRef`] ([`Endpoint::binding`]). Because the
//!   entries live in the realm document, `rkat`, REST and RPC resolve a TUI
//!   session's binding too. No session's realm chain names the sections, so
//!   they never change the default resolution of the built-in providers.
//! - **An OpenAI-dialect proxy is the engine's self-hosted server**
//!   ([`Endpoint::is_self_hosted`]): its connection lives in
//!   [`SELF_HOSTED_REALM`] with a `self_hosted` backend naming the server,
//!   the server is `self_hosted.servers.<endpoint>` (OpenAI-compatible,
//!   the Responses API), and every model it serves is the alias
//!   `<endpoint>:<model>` ([`Endpoint::engine_model`]) in
//!   `self_hosted.models`, with `remote_model` the model and every fact from
//!   the endpoint's listing ([`alias_entry`], [`ensure_aliases`]). An alias
//!   can never equal a catalog id, so a model the catalog also knows (a
//!   Claude id behind LiteLLM) runs on the endpoint's facts, not the
//!   catalog's row. rkat-tui names such a session's model as the endpoint
//!   lists it ([`from_engine`]).
//! - **Anthropic- and Gemini-dialect gateways** keep their dialect's client
//!   and connection in [`ENDPOINT_REALM`] (the engine has no alias for
//!   them): the provider's own client talks to the endpoint's URL with its
//!   key, and a catalogued model keeps the catalog's request shaping. A
//!   model no capability profile covers (a proxy alias, a model newer than
//!   the catalog) gets a `[models.<id>]` entry ([`ensure_model_profile`])
//!   built from what the endpoint reported (context window, output
//!   limit), recorded as rkat-tui's own ([`OwnedProfiles`]) and dropped
//!   again once the catalog defines the id ([`prune_catalogued_profiles`]).
//!   One `[models.<id>]` entry names one provider, so the same unknown
//!   model id on endpoints of different dialects is refused
//!   ([`EndpointError::DialectConflict`]).
//!
//! Which endpoint a session runs on is derived from its persisted identity
//! ([`endpoint_of`]): the binding names a configured endpoint, else the
//! provider names its built-in endpoint.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use meerkat_core::auth::{AuthConstraints, AuthMetadataDefaults};
use meerkat_core::config::{
    ConfigError, CustomModelConfig, SelfHostedApiStyle, SelfHostedModelConfig,
    SelfHostedServerConfig, SelfHostedTransport,
};
use meerkat_core::connection::{
    CredentialSourceSpec, IdentityError, RealmConfigSection, RealmConnectionSet, RealmId,
};
use meerkat_core::{
    AuthBindingRef, AuthProfileConfig, BackendProfileConfig, BindingId, BindingOrigin,
    BindingPolicy, Config, ConfigDelta, ConfigRuntime, ConfigRuntimeError, Provider,
    ProviderBindingConfig,
};

use crate::approval::rules::RuleOrigin;
use crate::bootstrap::credentials::{ProviderCredentials, resolve_builtin};
use crate::config::{EndpointSection, LayeredEndpoint, Notice};
use crate::models::ModelEntry;

/// Names of the built-in endpoints, in display order.
pub const BUILTIN_NAMES: [&str; 3] = ["anthropic", "openai", "gemini"];

/// The realm section holding one connection per configured endpoint (see
/// the module docs), written into the realm's own config document.
pub const ENDPOINT_REALM: &str = "rkat-tui-endpoints";

/// The realm section holding the connection of each self-hosted endpoint
/// ([`Endpoint::is_self_hosted`]): apart from [`ENDPOINT_REALM`], whose
/// entry of the same name a conversation stored by an earlier build may
/// still name with the endpoint's dialect provider (an entry is never
/// removed, so such a conversation keeps resolving as it was made).
pub const SELF_HOSTED_REALM: &str = "rkat-tui-self-hosted";

/// Whether `realm` is one of the sections rkat-tui projects its configured
/// endpoints into: a binding there names a configured endpoint.
#[must_use]
pub fn is_endpoint_realm(realm: &str) -> bool {
    realm == ENDPOINT_REALM || realm == SELF_HOSTED_REALM
}

/// The wire protocol an endpoint speaks: which of Meerkat's LLM clients
/// talks to it. A proxy (LiteLLM, vLLM, a gateway) speaks one of these
/// protocols whatever models it routes to, so a dialect names a protocol,
/// not who serves the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireDialect {
    /// Anthropic Messages API.
    Anthropic,
    /// OpenAI Responses API (`/v1/responses`); also LiteLLM and other proxies.
    OpenAi,
    /// Gemini `generateContent`.
    Gemini,
}

impl WireDialect {
    /// Every dialect, in display order.
    pub const ALL: [Self; 3] = [Self::Anthropic, Self::OpenAi, Self::Gemini];

    /// Canonical name (`anthropic`, `openai`, `gemini`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
        }
    }

    /// Parse a canonical name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.as_str() == name)
    }

    /// The dialect of a concrete provider's client.
    #[must_use]
    pub fn from_provider(provider: Provider) -> Option<Self> {
        match provider {
            Provider::Anthropic => Some(Self::Anthropic),
            Provider::OpenAI => Some(Self::OpenAi),
            Provider::Gemini => Some(Self::Gemini),
            Provider::SelfHosted | Provider::Other => None,
        }
    }

    /// The dialect of a catalogued model's provider
    /// ([`meerkat_models::infer_provider`]: exact catalog ids only, never a
    /// name prefix).
    #[must_use]
    pub fn of_catalog_model(model: &str) -> Option<Self> {
        meerkat_models::infer_provider(model).and_then(Self::from_provider)
    }
}

impl From<WireDialect> for Provider {
    fn from(dialect: WireDialect) -> Self {
        match dialect {
            WireDialect::Anthropic => Self::Anthropic,
            WireDialect::OpenAi => Self::OpenAI,
            WireDialect::Gemini => Self::Gemini,
        }
    }
}

impl fmt::Display for WireDialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A secret that never shows up in `Debug` output and has no `Display`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a secret.
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The secret itself; only for request headers, client construction and
    /// the realm document's inline credential source.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the secret is empty or blank.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The name of an endpoint (`anthropic`, `litellm`, …).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointName(String);

impl EndpointName {
    /// Wrap a name.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EndpointName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One place to send LLM requests.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Unique name (`anthropic`, `litellm`, …); the `endpoint` of `endpoint:model`.
    pub name: String,
    /// Protocol the endpoint speaks.
    pub dialect: WireDialect,
    /// Base URL override; `None` uses the dialect's public API.
    pub base_url: Option<String>,
    /// API key.
    pub api_key: SecretString,
    /// The environment variable the key came from (`api_key_env`), which
    /// the realm document's auth profile names; `None` only for a built-in
    /// endpoint.
    pub key_env: Option<String>,
    /// Whether this is one of the built-in provider endpoints.
    pub builtin: bool,
    /// Tie-break when several endpoints list the same bare model id
    /// (higher wins; built-ins are 0).
    pub priority: i64,
}

impl Endpoint {
    /// Credentials for the list-models request.
    #[must_use]
    pub fn credentials(&self) -> ProviderCredentials {
        ProviderCredentials {
            provider: self.dialect.into(),
            api_key: self.api_key.clone(),
            base_url: self.base_url.clone(),
        }
    }

    /// The provider whose client speaks this endpoint's dialect.
    #[must_use]
    pub fn provider(&self) -> Provider {
        self.dialect.into()
    }

    /// Whether the engine reaches this endpoint as one of its `self_hosted`
    /// servers: a configured OpenAI-dialect endpoint (a LiteLLM or vLLM
    /// proxy). Its models are endpoint-qualified aliases
    /// ([`Self::engine_model`]) whose facts all come from the endpoint.
    #[must_use]
    pub fn is_self_hosted(&self) -> bool {
        !self.builtin && self.dialect == WireDialect::OpenAi
    }

    /// The engine's model id of `model` on this endpoint: the alias
    /// `<endpoint>:<model>` on a self-hosted endpoint, else `model`.
    #[must_use]
    pub fn engine_model(&self, model: &str) -> String {
        if self.is_self_hosted() {
            EndpointRef::new(self.name.as_str(), model).to_string()
        } else {
            model.to_string()
        }
    }

    /// The engine provider of this endpoint's sessions:
    /// [`Provider::SelfHosted`] on a self-hosted endpoint, else the
    /// dialect's provider.
    #[must_use]
    pub fn engine_provider(&self) -> Provider {
        if self.is_self_hosted() {
            Provider::SelfHosted
        } else {
            self.provider()
        }
    }

    /// The engine's self-hosted server of this endpoint's sessions.
    #[must_use]
    pub fn self_hosted_server(&self) -> Option<String> {
        self.is_self_hosted().then(|| self.name.clone())
    }

    /// The explicit auth binding a session on this endpoint carries: `None`
    /// for a built-in endpoint (default resolution), the endpoint's
    /// connection in [`ENDPOINT_REALM`] otherwise.
    #[must_use]
    pub fn binding(&self) -> Option<AuthBindingRef> {
        if self.builtin {
            return None;
        }
        let realm = if self.is_self_hosted() {
            SELF_HOSTED_REALM
        } else {
            ENDPOINT_REALM
        };
        Some(AuthBindingRef {
            realm: RealmId::parse(realm).ok()?,
            binding: BindingId::parse(self.name.clone()).ok()?,
            profile: None,
            origin: BindingOrigin::Configured,
        })
    }

    /// Whether the TUI holds a key it can list models with.
    #[must_use]
    pub fn has_key(&self) -> bool {
        !self.api_key.is_blank()
    }

    /// `anthropic dialect` or `openai dialect, http://localhost:4000`.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn describe(&self) -> String {
        describe_endpoint(self.dialect, self.base_url.as_deref())
    }
}

/// `<dialect> dialect[, <base url>]`.
#[must_use]
pub fn describe_endpoint(dialect: WireDialect, base_url: Option<&str>) -> String {
    match base_url {
        Some(url) => format!("{dialect} dialect, {url}"),
        None => format!("{dialect} dialect"),
    }
}

/// A model on a named endpoint; displayed (and parsed) as `endpoint:model`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EndpointRef {
    /// Endpoint name.
    pub endpoint: String,
    /// Model id as the endpoint knows it.
    pub model: String,
}

impl EndpointRef {
    /// A reference to `model` on `endpoint`.
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
        }
    }
}

impl fmt::Display for EndpointRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.endpoint, self.model)
    }
}

/// A session's model and provider as rkat-tui names them, from the
/// engine's identity: a self-hosted alias `<endpoint>:<model>` is `model`
/// in the OpenAI dialect (the endpoint is its binding's); anything else
/// is as the engine has it.
#[must_use]
pub fn from_engine(model: &str, provider: &str) -> (String, String) {
    if provider == Provider::SelfHosted.as_str()
        && let Some((_, model)) = model.split_once(':')
    {
        return (model.to_string(), WireDialect::OpenAi.as_str().to_string());
    }
    (model.to_string(), provider.to_string())
}

/// Endpoint names, comma-separated (`none` when empty).
struct NameList<'a>(&'a [String]);

impl fmt::Display for NameList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("none");
        }
        for (i, name) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(name)?;
        }
        Ok(())
    }
}

/// A model reference that does not resolve to one endpoint.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelRefError {
    /// The reference was empty (or `endpoint:` without a model).
    #[error("empty model reference")]
    Empty,
    /// No endpoint is configured at all.
    #[error(
        "no LLM endpoint is configured: set ANTHROPIC_API_KEY, OPENAI_API_KEY or GEMINI_API_KEY, or add [endpoints.<name>] to tui.toml"
    )]
    NoEndpoints,
    /// `name:` (or `--endpoint name`) names no configured endpoint.
    #[error("unknown endpoint `{name}` (configured: {})", NameList(.known))]
    UnknownEndpoint {
        /// The name given.
        name: String,
        /// Configured endpoint names.
        known: Vec<String>,
    },
    /// A bare model id no fetched list and no catalog entry places on one
    /// endpoint.
    #[error(
        "cannot tell which endpoint serves `{model}` (no endpoint lists it and the catalog does not know it); use `<endpoint>:{model}` (configured: {})",
        NameList(.known)
    )]
    Ambiguous {
        /// The model id.
        model: String,
        /// Configured endpoint names.
        known: Vec<String>,
    },
    /// No model was given and none could be chosen.
    #[error(
        "no model selected and none can be chosen automatically; pass --model <endpoint>:<id> (configured: {})",
        NameList(.known)
    )]
    NoModel {
        /// Configured endpoint names.
        known: Vec<String>,
    },
}

/// Why an endpoint cannot be used, or its projection into the realm config
/// failed.
#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    /// A model reference did not resolve.
    #[error(transparent)]
    ModelRef(#[from] ModelRefError),
    /// The endpoint name is not a valid binding id.
    #[error("endpoint `{name}` cannot name a binding: {source}")]
    BindingName {
        /// The endpoint name.
        name: String,
        /// Why the binding id is invalid.
        #[source]
        source: IdentityError,
    },
    /// Upstream synthesizes no API-key backend for the dialect.
    #[error("no API-key backend for the {0} dialect")]
    NoApiKeyBackend(WireDialect),
    /// A configured endpoint carries its key inline (`api_key` in
    /// `tui.toml` or `--set`) instead of naming an environment variable.
    #[error(
        "endpoint `{name}` sets an inline `api_key`; keys are read from the environment only: remove `api_key` and set `api_key_env = \"<VAR>\"` in [endpoints.{name}] (or `--endpoint-def {name}=<dialect>=<url>,key_env=<VAR>`), naming the environment variable that holds the key"
    )]
    InlineApiKey {
        /// The endpoint.
        name: String,
    },
    /// One `[models.<id>]` profile names one provider: the same unknown
    /// model id cannot be served in two dialects.
    #[error(
        "model `{model}` is already registered for the {} dialect ([models.\"{model}\"] in the realm config); one profile cannot also describe it for {}: use the endpoint's own alias for one of them",
        .existing.as_str(),
        .requested.as_str()
    )]
    DialectConflict {
        /// The model id.
        model: String,
        /// The provider its profile names.
        existing: Provider,
        /// The provider the new endpoint needs.
        requested: Provider,
    },
    /// A `[models.<id>]` entry rkat-tui did not write profiles a model the
    /// engine's catalog defines: the engine refuses the realm's config.
    #[error(
        "the realm config's [models.\"{model}\"] profiles a model Meerkat's catalog now defines, so the engine refuses the config; remove that entry from the realm's config.toml (rkat-tui did not write it, so it does not remove it)"
    )]
    ProfileShadowsCatalog {
        /// The model id.
        model: String,
    },
    /// The record of the profiles rkat-tui wrote cannot be read or written.
    #[error("{}: {why}", .file.display())]
    OwnedProfiles {
        /// The record.
        file: std::path::PathBuf,
        /// Why.
        why: String,
    },
    /// The effective model registry could not be built.
    #[error("model registry: {0}")]
    Registry(#[source] ConfigError),
    /// The realm's config document could not be read.
    #[error("cannot read the realm config: {0}")]
    ConfigRead(#[source] ConfigError),
    /// The realm's config runtime could not read the document (or its
    /// generation) for a write.
    #[error("cannot read the realm config for a write: {0}")]
    ConfigSnapshot(#[source] ConfigRuntimeError),
    /// The realm's config runtime refused the write.
    #[error("cannot write the realm config: {0}")]
    ConfigWrite(#[source] ConfigRuntimeError),
    /// Another writer changed the realm config between each of rkat-tui's
    /// reads and its write.
    #[error("the realm config kept changing under rkat-tui's write ({attempts} attempts)")]
    ConfigContended {
        /// Reads and writes tried.
        attempts: u32,
    },
    /// The patch for the realm document could not be built.
    #[error("cannot encode the realm config patch: {0}")]
    Encode(#[source] serde_json::Error),
    /// The endpoint to take the first model of lists none.
    #[error(
        "no model selected and endpoint `{endpoint}` offers none ({why}); pass --model {endpoint}:<id>"
    )]
    NothingListed {
        /// The endpoint.
        endpoint: String,
        /// Why its list is empty or missing.
        why: String,
    },
}

fn names(endpoints: &[Endpoint]) -> Vec<String> {
    endpoints.iter().map(|e| e.name.clone()).collect()
}

/// Split `name:model` when `name` is a configured endpoint.
fn split_explicit<'a>(s: &'a str, endpoints: &[Endpoint]) -> Option<(&'a str, &'a str)> {
    let (name, model) = s.split_once(':')?;
    endpoints
        .iter()
        .any(|e| e.name == name)
        .then_some((name, model))
}

/// The endpoint serving a catalogued id: the built-in endpoint of its
/// provider's dialect when configured, else the only endpoint of that
/// dialect. Ids outside the catalog never resolve here.
fn by_catalog<'a>(model: &str, endpoints: &'a [Endpoint]) -> Option<&'a Endpoint> {
    let dialect = WireDialect::of_catalog_model(model)?;
    if let Some(builtin) = endpoints.iter().find(|e| e.builtin && e.dialect == dialect) {
        return Some(builtin);
    }
    let mut same = endpoints.iter().filter(|e| e.dialect == dialect);
    match (same.next(), same.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// Which endpoints list which model ids (from the fetched model lists).
#[derive(Debug, Clone, Default)]
pub struct ModelIndex {
    /// Model id → endpoints listing it, in insertion order.
    listed: HashMap<String, Vec<String>>,
}

impl ModelIndex {
    /// An index over `(endpoint, models)` pairs.
    pub fn from_lists<'a>(lists: impl IntoIterator<Item = (&'a str, &'a [ModelEntry])>) -> Self {
        let mut index = Self::default();
        for (endpoint, models) in lists {
            index.insert(endpoint, models.iter().map(|m| m.id.as_str()));
        }
        index
    }

    /// Record that `endpoint` lists `ids`.
    pub fn insert<'a>(&mut self, endpoint: &str, ids: impl IntoIterator<Item = &'a str>) {
        for id in ids {
            let entry = self.listed.entry(id.to_string()).or_default();
            if !entry.iter().any(|e| e == endpoint) {
                entry.push(endpoint.to_string());
            }
        }
    }

    /// Whether `endpoint` lists `id`.
    #[must_use]
    pub fn lists(&self, endpoint: &str, id: &str) -> bool {
        self.listed
            .get(id)
            .is_some_and(|endpoints| endpoints.iter().any(|e| e == endpoint))
    }

    /// Whether nothing is indexed.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn is_empty(&self) -> bool {
        self.listed.is_empty()
    }
}

/// Whether resolving `s` would consult the model lists: it is a bare id and
/// more than one endpoint could serve it.
#[must_use]
pub fn needs_lookup(s: &str, endpoints: &[Endpoint]) -> bool {
    endpoints.len() > 1 && split_explicit(s.trim(), endpoints).is_none()
}

/// Pick among several endpoints listing the same id: the current endpoint,
/// then the built-in endpoint of the catalogued id's dialect, then the
/// highest `priority`, then the first configured.
fn tie_break<'a>(
    model: &str,
    candidates: &[&'a Endpoint],
    current: Option<&str>,
) -> Option<&'a Endpoint> {
    if let Some(current) = current
        && let Some(found) = candidates.iter().find(|e| e.name == current)
    {
        return Some(found);
    }
    if let Some(dialect) = WireDialect::of_catalog_model(model)
        && let Some(found) = candidates
            .iter()
            .find(|e| e.builtin && e.dialect == dialect)
    {
        return Some(found);
    }
    let best = candidates.iter().map(|e| e.priority).max()?;
    candidates.iter().find(|e| e.priority == best).copied()
}

/// Parse a model reference.
///
/// `name:id` is explicit when `name` is a configured endpoint (model ids may
/// themselves contain `:`). A bare id is looked up in `index` (the fetched
/// model lists): one endpoint listing it → that endpoint; several → the
/// `current` endpoint, then the built-in endpoint of the id's catalog
/// dialect, then the highest `priority`, then the first configured. When no
/// endpoint lists it, a catalogued id goes to the built-in endpoint of its
/// provider's dialect (else the only endpoint of that dialect), and any id
/// to the only configured endpoint. Ids are never inferred from a name
/// prefix: outside the catalog, only the live lists place a bare id.
///
/// # Errors
///
/// [`ModelRefError`] when the reference is empty or does not pick exactly one
/// endpoint; the error lists the configured endpoints.
pub fn parse_model_ref(
    s: &str,
    endpoints: &[Endpoint],
    index: &ModelIndex,
    current: Option<&str>,
) -> Result<EndpointRef, ModelRefError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ModelRefError::Empty);
    }
    if endpoints.is_empty() {
        return Err(ModelRefError::NoEndpoints);
    }
    if let Some((name, model)) = split_explicit(s, endpoints) {
        if model.is_empty() {
            return Err(ModelRefError::Empty);
        }
        return Ok(EndpointRef::new(name, model));
    }
    let listing: Vec<&Endpoint> = endpoints
        .iter()
        .filter(|e| index.lists(&e.name, s))
        .collect();
    if let Some(endpoint) = tie_break(s, &listing, current) {
        return Ok(EndpointRef::new(endpoint.name.clone(), s));
    }
    if let Some(endpoint) = by_catalog(s, endpoints) {
        return Ok(EndpointRef::new(endpoint.name.clone(), s));
    }
    if let [only] = endpoints {
        return Ok(EndpointRef::new(only.name.clone(), s));
    }
    // `foo:bar` where `foo` looks like an endpoint name the user forgot to configure.
    if let Some((name, _)) = s.split_once(':')
        && is_valid_endpoint_name(name)
    {
        return Err(ModelRefError::UnknownEndpoint {
            name: name.to_string(),
            known: names(endpoints),
        });
    }
    Err(ModelRefError::Ambiguous {
        model: s.to_string(),
        known: names(endpoints),
    })
}

/// Whether `name` is usable as an endpoint name: ASCII letters, digits, `_`, `-`.
#[must_use]
pub fn is_valid_endpoint_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

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
                endpoints.push(Endpoint {
                    name: dialect.as_str().to_string(),
                    dialect,
                    base_url: creds.base_url,
                    api_key: creds.api_key.unwrap_or_default(),
                    key_env: None,
                    builtin: true,
                    priority: 0,
                });
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
                    Endpoint {
                        name: dialect.as_str().to_string(),
                        dialect,
                        base_url: None,
                        api_key: SecretString::default(),
                        key_env: None,
                        builtin: true,
                        priority: 0,
                    },
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
}

/// Validate one `[endpoints.<name>]` entry (an inline `api_key` is refused
/// before, by [`EndpointRegistry::discover_with`]).
fn configured_endpoint(
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
    let Some(var) = api_key_env else {
        return Err(EndpointSkip::NoKey);
    };
    if layered.origin == RuleOrigin::Project {
        return Err(EndpointSkip::ProjectEnvKey(var.clone()));
    }
    let key = env(var)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| EndpointSkip::UnsetEnv(var.clone()))?;
    Ok(Endpoint {
        name: name.to_string(),
        dialect,
        base_url: base_url.clone(),
        api_key: SecretString::new(key),
        key_env: Some(var.clone()),
        builtin: false,
        priority: priority.unwrap_or(0),
    })
}

/// The backend kind of a self-hosted endpoint's connection.
const SELF_HOSTED_BACKEND: &str = "openai_compatible";
/// The auth method of a self-hosted endpoint's connection (its key, sent
/// as a bearer token).
const SELF_HOSTED_AUTH: &str = "api_key";

/// What rkat-tui projects into the realm document for its configured
/// endpoints: their [`ENDPOINT_REALM`] connections and, for the
/// self-hosted ones, their `self_hosted.servers` entries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EndpointProjection {
    /// The [`ENDPOINT_REALM`] connections ([`endpoint_section`]).
    pub section: RealmConfigSection,
    /// The [`SELF_HOSTED_REALM`] connections ([`self_hosted_section`]).
    pub self_hosted: RealmConfigSection,
    /// The servers ([`self_hosted_servers`]).
    pub servers: BTreeMap<String, SelfHostedServerConfig>,
}

impl EndpointProjection {
    /// The projection of `endpoints`.
    ///
    /// # Errors
    ///
    /// As [`endpoint_section`].
    pub fn of(endpoints: &[Endpoint]) -> Result<Self, EndpointError> {
        Ok(Self {
            section: endpoint_section(endpoints)?,
            self_hosted: self_hosted_section(endpoints)?,
            servers: self_hosted_servers(endpoints),
        })
    }

    /// The projected connections of the realm section `realm`.
    #[must_use]
    pub fn section_of(&self, realm: &str) -> Option<&RealmConfigSection> {
        match realm {
            ENDPOINT_REALM => Some(&self.section),
            SELF_HOSTED_REALM => Some(&self.self_hosted),
            _ => None,
        }
    }
}

/// The `self_hosted.servers` entry of each self-hosted endpoint of
/// `endpoints` ([`Endpoint::is_self_hosted`]): the OpenAI-compatible
/// transport on the endpoint's URL (the public OpenAI API without one),
/// speaking the Responses API as rkat-tui's OpenAI dialect always has.
#[must_use]
pub fn self_hosted_servers(endpoints: &[Endpoint]) -> BTreeMap<String, SelfHostedServerConfig> {
    endpoints
        .iter()
        .filter(|e| e.is_self_hosted())
        .map(|endpoint| {
            let base_url = endpoint.base_url.clone().unwrap_or_else(|| {
                meerkat_core::provider_matrix::OpenAiBackendKind::OpenAiApi
                    .default_base_url()
                    .to_string()
            });
            (
                endpoint.name.clone(),
                SelfHostedServerConfig {
                    transport: SelfHostedTransport::OpenAiCompatible,
                    base_url,
                    api_style: SelfHostedApiStyle::Responses,
                },
            )
        })
        .collect()
}

/// The [`SELF_HOSTED_REALM`] section entries of the self-hosted endpoints
/// of `endpoints`.
///
/// # Errors
///
/// As [`endpoint_section`].
pub fn self_hosted_section(endpoints: &[Endpoint]) -> Result<RealmConfigSection, EndpointError> {
    let self_hosted: Vec<Endpoint> = endpoints
        .iter()
        .filter(|e| e.is_self_hosted())
        .cloned()
        .collect();
    connections(&self_hosted)
}

/// The [`ENDPOINT_REALM`] section entries of the configured endpoints of
/// `endpoints` that are not self-hosted (built-in endpoints add nothing;
/// the self-hosted ones are [`self_hosted_section`]'s).
///
/// # Errors
///
/// As [`connections`].
pub fn endpoint_section(endpoints: &[Endpoint]) -> Result<RealmConfigSection, EndpointError> {
    let dialect: Vec<Endpoint> = endpoints
        .iter()
        .filter(|e| !e.is_self_hosted())
        .cloned()
        .collect();
    connections(&dialect)
}

/// One connection (backend, auth profile, binding, each named after the
/// endpoint) per configured endpoint of `endpoints`.
///
/// # Errors
///
/// [`EndpointError::BindingName`] when an endpoint name is not a valid
/// binding id, [`EndpointError::NoApiKeyBackend`] when upstream names no
/// API-key backend for a dialect, [`EndpointError::InlineApiKey`] when a
/// configured endpoint names no environment variable (the document never
/// holds a key).
fn connections(endpoints: &[Endpoint]) -> Result<RealmConfigSection, EndpointError> {
    let mut section = RealmConfigSection::default();
    for endpoint in endpoints.iter().filter(|e| !e.builtin) {
        BindingId::parse(endpoint.name.clone()).map_err(|source| EndpointError::BindingName {
            name: endpoint.name.clone(),
            source,
        })?;
        let provider = endpoint.engine_provider();
        let (backend_kind, auth_method) = if endpoint.is_self_hosted() {
            // The engine's OpenAI-compatible self-hosted transport, keyed
            // with the endpoint's key; the backend names the server it
            // authenticates, so the engine sends this key there only.
            (
                SELF_HOSTED_BACKEND.to_string(),
                SELF_HOSTED_AUTH.to_string(),
            )
        } else {
            // The provider's own API-key backend and auth method, as the
            // environment default would name them.
            let synthetic =
                RealmConnectionSet::synthesize_env_default_from_lookup(provider, |_| None);
            let (Some(backend), Some(auth)) = (
                synthetic.backends.values().next(),
                synthetic.auth_profiles.values().next(),
            ) else {
                return Err(EndpointError::NoApiKeyBackend(endpoint.dialect));
            };
            (backend.backend_kind.clone(), auth.auth_method.clone())
        };
        let Some(env) = &endpoint.key_env else {
            return Err(EndpointError::InlineApiKey {
                name: endpoint.name.clone(),
            });
        };
        let source = CredentialSourceSpec::Env {
            env: env.clone(),
            fallback: Vec::new(),
        };
        section.backend.insert(
            endpoint.name.clone(),
            BackendProfileConfig {
                provider: provider.as_str().to_string(),
                backend_kind,
                base_url: endpoint.base_url.clone(),
                options: serde_json::Value::Null,
                server: endpoint.self_hosted_server(),
            },
        );
        section.auth.insert(
            endpoint.name.clone(),
            AuthProfileConfig {
                provider: provider.as_str().to_string(),
                auth_method,
                source,
                constraints: AuthConstraints::default(),
                metadata_defaults: AuthMetadataDefaults::default(),
            },
        );
        section.binding.insert(
            endpoint.name.clone(),
            ProviderBindingConfig {
                backend_profile: endpoint.name.clone(),
                auth_profile: endpoint.name.clone(),
                credential_account: None,
                default_model: None,
                policy: BindingPolicy::default(),
                provider_default: false,
            },
        );
    }
    Ok(section)
}

/// The merge patch that makes the realm document's [`ENDPOINT_REALM`]
/// section hold `section`'s entries, or `None` when `config` (the document
/// as persisted) already holds every one of them unchanged. Entries of
/// endpoints no longer configured stay, so sessions bound to them keep
/// resolving for every surface.
///
/// # Errors
///
/// [`EndpointError::Encode`] when an entry cannot be encoded.
pub fn endpoint_patch(
    config: &Config,
    projection: &EndpointProjection,
) -> Result<Option<ConfigDelta>, EndpointError> {
    let mut realms = serde_json::Map::new();
    for (realm, section) in [
        (ENDPOINT_REALM, &projection.section),
        (SELF_HOSTED_REALM, &projection.self_hosted),
    ] {
        let existing = config.realm.get(realm);
        let backend = changed(&section.backend, existing.map(|s| &s.backend))?;
        let auth = changed(&section.auth, existing.map(|s| &s.auth))?;
        let binding = changed(&section.binding, existing.map(|s| &s.binding))?;
        let mut realm_section = serde_json::Map::new();
        for (key, map) in [("backend", backend), ("auth", auth), ("binding", binding)] {
            if !map.is_empty() {
                realm_section.insert(key.to_string(), serde_json::Value::Object(map));
            }
        }
        if !realm_section.is_empty() {
            realms.insert(realm.to_string(), serde_json::Value::Object(realm_section));
        }
    }
    let servers = changed(&projection.servers, Some(&config.self_hosted.servers))?;
    let mut patch = serde_json::Map::new();
    if !realms.is_empty() {
        patch.insert("realm".to_string(), serde_json::Value::Object(realms));
    }
    if !servers.is_empty() {
        patch.insert(
            "self_hosted".to_string(),
            serde_json::json!({ "servers": servers }),
        );
    }
    if patch.is_empty() {
        return Ok(None);
    }
    Ok(Some(ConfigDelta(serde_json::Value::Object(patch))))
}

/// The entries of `want` that `have` lacks or holds differently, encoded.
fn changed<T: serde::Serialize>(
    want: &BTreeMap<String, T>,
    have: Option<&BTreeMap<String, T>>,
) -> Result<serde_json::Map<String, serde_json::Value>, EndpointError> {
    let mut out = serde_json::Map::new();
    for (name, entry) in want {
        let encoded = serde_json::to_value(entry).map_err(EndpointError::Encode)?;
        let current = have
            .and_then(|have| have.get(name))
            .map(serde_json::to_value)
            .transpose()
            .map_err(EndpointError::Encode)?;
        if current.as_ref() != Some(&encoded) {
            out.insert(name.clone(), encoded);
        }
    }
    Ok(out)
}

/// How often [`patch_document`] re-reads after a generation conflict.
const CONFIG_WRITE_ATTEMPTS: u32 = 8;

/// Read the realm document through `runtime`, let `delta_for` build the
/// patch from it (`None`: nothing to write), and write it under the
/// generation read, re-reading when another writer got in between (the
/// read-modify-write contract `rkat config` follows). Returns whether the
/// document changed.
///
/// # Errors
///
/// What `delta_for` returns, [`EndpointError::ConfigSnapshot`] /
/// [`EndpointError::ConfigWrite`] when the runtime cannot read or write,
/// [`EndpointError::ConfigContended`] when every attempt conflicted.
pub async fn patch_document(
    runtime: &ConfigRuntime,
    delta_for: impl Fn(&Config) -> Result<Option<ConfigDelta>, EndpointError>,
) -> Result<bool, EndpointError> {
    for _ in 0..CONFIG_WRITE_ATTEMPTS {
        let snapshot = runtime.get().await.map_err(EndpointError::ConfigSnapshot)?;
        let Some(delta) = delta_for(&snapshot.config)? else {
            return Ok(false);
        };
        match runtime.patch(delta, Some(snapshot.generation)).await {
            Ok(_) => return Ok(true),
            Err(ConfigRuntimeError::GenerationConflict { .. }) => {}
            Err(err) => return Err(EndpointError::ConfigWrite(err)),
        }
    }
    Err(EndpointError::ConfigContended {
        attempts: CONFIG_WRITE_ATTEMPTS,
    })
}

/// Project the configured endpoints of `endpoints` into the realm document
/// through its config `runtime` (see the module docs). Returns whether the
/// document changed. The document names each endpoint's environment
/// variable, never a key.
///
/// # Errors
///
/// [`EndpointError`] when the section cannot be built or the write fails
/// (see [`patch_document`]).
pub async fn project_endpoints(
    runtime: &ConfigRuntime,
    endpoints: &[Endpoint],
) -> Result<bool, EndpointError> {
    let projection = EndpointProjection::of(endpoints)?;
    if projection.section.binding.is_empty() && projection.self_hosted.binding.is_empty() {
        return Ok(false);
    }
    patch_document(runtime, |persisted| endpoint_patch(persisted, &projection)).await
}

/// The `[models.<id>]` entry `model` on `provider` needs, or `None` when
/// the catalog or the realm config already profiles it the same way. A
/// custom entry is refreshed when the endpoint now reports a window or an
/// output limit it lacked or that changed.
///
/// # Errors
///
/// [`EndpointError::Registry`] when the config's model registry is invalid,
/// [`EndpointError::DialectConflict`] when the id is already registered for
/// another provider.
pub fn model_profile_entry(
    config: &Config,
    provider: Provider,
    model: &str,
    listed: Option<&ModelEntry>,
) -> Result<Option<CustomModelConfig>, EndpointError> {
    let clamp = |value: Option<u64>| value.and_then(|v| u32::try_from(v).ok());
    if let Some(existing) = config.models.custom.get(model) {
        if existing.provider != provider {
            return Err(EndpointError::DialectConflict {
                model: model.to_string(),
                existing: existing.provider,
                requested: provider,
            });
        }
        let Some(listed) = listed else {
            return Ok(None);
        };
        let mut refreshed = existing.clone();
        if let Some(window) = clamp(listed.context_window) {
            refreshed.context_window = Some(window);
        }
        if let Some(output) = clamp(listed.max_output_tokens) {
            refreshed.max_output_tokens = Some(output);
        }
        return Ok((&refreshed != existing).then_some(refreshed));
    }
    let registry = config
        .model_registry(meerkat_models::canonical())
        .map_err(EndpointError::Registry)?;
    if registry.profile_for_provider(provider, model).is_some() {
        return Ok(None);
    }
    if let Some(other) = meerkat_models::infer_provider(model)
        && other != provider
    {
        return Err(EndpointError::DialectConflict {
            model: model.to_string(),
            existing: other,
            requested: provider,
        });
    }
    Ok(Some(CustomModelConfig {
        provider,
        display_name: listed.and_then(|entry| entry.display_name.clone()),
        context_window: clamp(listed.and_then(|entry| entry.context_window)),
        max_input_tokens: None,
        max_output_tokens: clamp(listed.and_then(|entry| entry.max_output_tokens)),
        vision: None,
        web_search: None,
        call_timeout_secs: None,
    }))
}

/// Make sure the realm document profiles `model` on `provider`
/// ([`model_profile_entry`]), writing the `[models.<id>]` entry through its
/// config `runtime`. An entry it creates is recorded in `owned` first, so
/// that [`prune_catalogued_profiles`] may drop it once the catalog defines
/// the id. Returns whether the document changed.
///
/// # Errors
///
/// As [`model_profile_entry`] and [`patch_document`], and
/// [`EndpointError::OwnedProfiles`] when the record cannot be written.
pub async fn ensure_model_profile(
    runtime: &ConfigRuntime,
    owned: &OwnedProfiles,
    provider: Provider,
    model: &str,
    listed: Option<&ModelEntry>,
) -> Result<bool, EndpointError> {
    patch_document(runtime, |persisted| {
        let Some(entry) = model_profile_entry(persisted, provider, model, listed)? else {
            return Ok(None);
        };
        if !persisted.models.custom.contains_key(model) {
            owned.add(model)?;
        }
        let value = serde_json::to_value(&entry).map_err(EndpointError::Encode)?;
        Ok(Some(ConfigDelta(
            serde_json::json!({ "models": { model: value } }),
        )))
    })
    .await
}

/// The `[models.<id>]` entries rkat-tui wrote into a realm document (for
/// models only an endpoint knew), recorded in the realm's
/// [`OWNED_PROFILES_FILE`]. An entry not recorded there is the user's.
#[derive(Debug, Clone)]
pub struct OwnedProfiles {
    file: std::path::PathBuf,
}

/// The file in a realm's directory that names the `[models.<id>]` entries
/// rkat-tui wrote ([`OwnedProfiles`]).
pub const OWNED_PROFILES_FILE: &str = "tui_models.json";

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedProfilesRecord {
    profiles: std::collections::BTreeSet<String>,
}

impl OwnedProfiles {
    /// The record of the realm at `realm_root`.
    #[must_use]
    pub fn in_realm(realm_root: &std::path::Path) -> Self {
        Self {
            file: realm_root.join(OWNED_PROFILES_FILE),
        }
    }

    /// The ids recorded (none without a record).
    ///
    /// # Errors
    ///
    /// [`EndpointError::OwnedProfiles`] when the record cannot be read.
    pub fn load(&self) -> Result<std::collections::BTreeSet<String>, EndpointError> {
        let text = match std::fs::read_to_string(&self.file) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(std::collections::BTreeSet::new());
            }
            Err(err) => return Err(self.error(err.to_string())),
        };
        serde_json::from_str::<OwnedProfilesRecord>(&text)
            .map(|record| record.profiles)
            .map_err(|err| self.error(err.to_string()))
    }

    fn add(&self, model: &str) -> Result<(), EndpointError> {
        let mut profiles = self.load()?;
        if profiles.insert(model.to_string()) {
            self.save(profiles)?;
        }
        Ok(())
    }

    fn remove(&self, models: &[String]) -> Result<(), EndpointError> {
        let mut profiles = self.load()?;
        let before = profiles.len();
        profiles.retain(|id| !models.contains(id));
        if profiles.len() != before {
            self.save(profiles)?;
        }
        Ok(())
    }

    fn save(&self, profiles: std::collections::BTreeSet<String>) -> Result<(), EndpointError> {
        let text = serde_json::to_string_pretty(&OwnedProfilesRecord { profiles })
            .map_err(EndpointError::Encode)?;
        let partial = self.file.with_extension("json.partial");
        std::fs::write(&partial, text)
            .and_then(|()| std::fs::rename(&partial, &self.file))
            .map_err(|err| self.error(err.to_string()))
    }

    fn error(&self, why: String) -> EndpointError {
        EndpointError::OwnedProfiles {
            file: self.file.clone(),
            why,
        }
    }
}

/// Drop from the realm document every `[models.<id>]` entry rkat-tui wrote
/// ([`OwnedProfiles`]) whose id the engine's catalog now defines: the
/// engine refuses a document that profiles a catalogued id again, so a
/// catalog addition would otherwise stop every write and every session
/// build in the realm. Returns the ids dropped.
///
/// # Errors
///
/// [`EndpointError::ProfileShadowsCatalog`] for such an entry the user
/// wrote (it is never dropped silently), [`EndpointError::OwnedProfiles`]
/// when the record cannot be read or written, and as [`patch_document`].
pub async fn prune_catalogued_profiles(
    runtime: &ConfigRuntime,
    owned: &OwnedProfiles,
) -> Result<Vec<String>, EndpointError> {
    let dropped = parking_lot::Mutex::new(Vec::new());
    patch_document(runtime, |persisted| {
        let catalogued: Vec<String> = persisted
            .models
            .custom
            .keys()
            .filter(|id| meerkat_models::infer_provider(id).is_some())
            .cloned()
            .collect();
        if catalogued.is_empty() {
            return Ok(None);
        }
        let mine = owned.load()?;
        if let Some(theirs) = catalogued.iter().find(|id| !mine.contains(*id)) {
            return Err(EndpointError::ProfileShadowsCatalog {
                model: theirs.clone(),
            });
        }
        let nulls: serde_json::Map<String, serde_json::Value> = catalogued
            .iter()
            .map(|id| (id.clone(), serde_json::Value::Null))
            .collect();
        dropped.lock().clone_from(&catalogued);
        Ok(Some(ConfigDelta(serde_json::json!({ "models": nulls }))))
    })
    .await?;
    let dropped = dropped.into_inner();
    owned.remove(&dropped)?;
    Ok(dropped)
}

/// The start-up notice for the profiles [`prune_catalogued_profiles`]
/// dropped, `None` when it dropped none.
#[must_use]
pub fn pruned_notice(dropped: &[String]) -> Option<String> {
    (!dropped.is_empty()).then(|| {
        let names: Vec<String> = dropped
            .iter()
            .map(|id| format!("[models.\"{id}\"]"))
            .collect();
        format!(
            "removed {} from the realm config: rkat-tui wrote it for a model only an endpoint knew, and Meerkat's model catalog now defines that model",
            names.join(", ")
        )
    })
}

/// The `self_hosted.models` entry of `model` on the self-hosted `endpoint`
/// ([`Endpoint::engine_model`] is its key): every fact from the endpoint's
/// listing (`listed`), conservative where it says nothing (no thinking,
/// no reasoning, no images), sampling temperature allowed.
#[must_use]
pub fn alias_entry(
    endpoint: &Endpoint,
    model: &str,
    listed: Option<&ModelEntry>,
) -> SelfHostedModelConfig {
    let clamp = |value: Option<u64>| value.and_then(|v| u32::try_from(v).ok());
    let capabilities = listed.map(|entry| entry.capabilities).unwrap_or_default();
    let reasoning = capabilities.reasoning.unwrap_or(false);
    let vision = capabilities.vision.unwrap_or(false);
    SelfHostedModelConfig {
        server: endpoint.name.clone(),
        remote_model: model.to_string(),
        display_name: listed
            .and_then(|entry| entry.display_name.clone())
            .unwrap_or_else(|| model.to_string()),
        context_window: clamp(listed.and_then(|entry| entry.context_window)),
        max_output_tokens: clamp(listed.and_then(|entry| entry.max_output_tokens)),
        vision,
        image_tool_results: vision,
        supports_temperature: true,
        supports_thinking: reasoning,
        supports_reasoning: reasoning,
        ..SelfHostedModelConfig::default()
    }
}

/// The merge patch that gives the realm document a `self_hosted.models`
/// alias for each `(model, listing)` of `models` on the self-hosted
/// `endpoint`, or `None` when it already holds each one as the listing
/// says (an alias with no listing now is left as it is). The first alias
/// becomes `self_hosted.default_model` when the document names none (the
/// engine requires one once there are two).
///
/// # Errors
///
/// [`EndpointError::Encode`] when an entry cannot be encoded.
pub fn alias_patch(
    config: &Config,
    endpoint: &Endpoint,
    models: &[(&str, Option<&ModelEntry>)],
) -> Result<Option<ConfigDelta>, EndpointError> {
    let mut wanted = BTreeMap::new();
    for (model, listed) in models {
        let alias = endpoint.engine_model(model);
        let existing = config.self_hosted.models.get(&alias);
        if existing.is_some() && listed.is_none() {
            continue;
        }
        let entry = alias_entry(endpoint, model, *listed);
        if existing != Some(&entry) {
            wanted.insert(alias, entry);
        }
    }
    let mut patch = serde_json::Map::new();
    if !wanted.is_empty() {
        let encoded = serde_json::to_value(&wanted).map_err(EndpointError::Encode)?;
        patch.insert("models".to_string(), encoded);
    }
    let default_valid = config
        .self_hosted
        .default_model
        .as_ref()
        .is_some_and(|model| config.self_hosted.models.contains_key(model));
    if !default_valid && let Some((model, _)) = models.first() {
        patch.insert(
            "default_model".to_string(),
            serde_json::Value::String(endpoint.engine_model(model)),
        );
    }
    if patch.is_empty() {
        return Ok(None);
    }
    Ok(Some(ConfigDelta(serde_json::json!({
        "self_hosted": patch
    }))))
}

/// Make sure the realm document holds the self-hosted aliases of `models`
/// on `endpoint` ([`alias_patch`]). Returns whether the document changed.
///
/// # Errors
///
/// As [`alias_patch`] and [`patch_document`].
pub async fn ensure_aliases(
    runtime: &ConfigRuntime,
    endpoint: &Endpoint,
    models: &[(&str, Option<&ModelEntry>)],
) -> Result<bool, EndpointError> {
    if !endpoint.is_self_hosted() || models.is_empty() {
        return Ok(false);
    }
    patch_document(runtime, |persisted| {
        alias_patch(persisted, endpoint, models)
    })
    .await
}

/// Make sure the engine can run `model` on `endpoint`: its self-hosted
/// alias ([`ensure_aliases`]) on a self-hosted endpoint, else its
/// `[models.<id>]` profile when it needs one ([`ensure_model_profile`]).
/// Returns whether the document changed.
///
/// # Errors
///
/// As [`ensure_aliases`] and [`ensure_model_profile`].
pub async fn ensure_endpoint_model(
    runtime: &ConfigRuntime,
    owned: &OwnedProfiles,
    endpoint: &Endpoint,
    model: &str,
    listed: Option<&ModelEntry>,
) -> Result<bool, EndpointError> {
    if endpoint.is_self_hosted() {
        ensure_aliases(runtime, endpoint, &[(model, listed)]).await
    } else {
        ensure_model_profile(runtime, owned, endpoint.provider(), model, listed).await
    }
}

/// The context window the engine's model registry knows for `model` on
/// `provider` (the catalog or a `[models.<id>]` entry, which listings keep
/// current): the window the gauge and the compaction policy use.
#[must_use]
pub fn registry_context_window(config: &Config, provider: Provider, model: &str) -> Option<u64> {
    let registry = config.model_registry(meerkat_models::canonical()).ok()?;
    registry
        .entry_for_provider(provider, model)
        .and_then(|entry| entry.context_window)
        .map(u64::from)
}

/// What the start-up flags and config ask for.
#[derive(Debug, Clone, Copy, Default)]
pub struct StartupRequest<'a> {
    /// `--model` (`endpoint:id` or a bare id).
    pub model: Option<&'a str>,
    /// `--endpoint`: endpoint for a bare id and the default.
    pub endpoint: Option<&'a str>,
    /// The realm config's `agent.model` (may be empty).
    pub config_model: &'a str,
}

/// The start-up model, or the endpoint whose list must be fetched to pick one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupPlan {
    /// The model is known.
    Resolved(EndpointRef),
    /// Use the first model `endpoint` lists.
    FirstListed(String),
}

/// Decide the start-up model without network access.
///
/// - `--model` wins: explicit `endpoint:id`, else a bare id on `--endpoint`
///   when given, else [`parse_model_ref`] over `index` (fetch the lists first
///   when [`needs_lookup`] says so).
/// - `--endpoint` alone: a built-in endpoint uses the config model when the
///   catalog places it on that endpoint's provider, else the catalog
///   default; a configured endpoint uses the first model it lists.
/// - Neither: the config model when the catalog places it on an endpoint,
///   else (one endpoint) the catalog default of a built-in or the first
///   listed model.
///
/// # Errors
///
/// [`ModelRefError`] (listing the endpoints) when nothing resolves.
pub fn plan_startup(
    request: StartupRequest<'_>,
    endpoints: &[Endpoint],
    index: &ModelIndex,
) -> Result<StartupPlan, ModelRefError> {
    if endpoints.is_empty() {
        return Err(ModelRefError::NoEndpoints);
    }
    let selected = request
        .endpoint
        .map(|name| {
            endpoints.iter().find(|e| e.name == name).ok_or_else(|| {
                ModelRefError::UnknownEndpoint {
                    name: name.to_string(),
                    known: names(endpoints),
                }
            })
        })
        .transpose()?;
    if let Some(model) = request.model {
        if let Some((name, id)) = split_explicit(model, endpoints) {
            if id.is_empty() {
                return Err(ModelRefError::Empty);
            }
            return Ok(StartupPlan::Resolved(EndpointRef::new(name, id)));
        }
        return match selected {
            Some(endpoint) if !model.trim().is_empty() => Ok(StartupPlan::Resolved(
                EndpointRef::new(endpoint.name.clone(), model.trim()),
            )),
            _ => parse_model_ref(model, endpoints, index, None).map(StartupPlan::Resolved),
        };
    }
    let config_model = request.config_model.trim();
    if let Some(endpoint) = selected {
        if !endpoint.builtin {
            return Ok(StartupPlan::FirstListed(endpoint.name.clone()));
        }
        return Ok(StartupPlan::Resolved(builtin_default(
            endpoint,
            config_model,
        )));
    }
    if !config_model.is_empty() {
        if let Some((name, id)) = split_explicit(config_model, endpoints)
            && !id.is_empty()
        {
            return Ok(StartupPlan::Resolved(EndpointRef::new(name, id)));
        }
        if let Some(endpoint) = by_catalog(config_model, endpoints) {
            return Ok(StartupPlan::Resolved(EndpointRef::new(
                endpoint.name.clone(),
                config_model,
            )));
        }
    }
    match endpoints {
        [only] if only.builtin => Ok(StartupPlan::Resolved(builtin_default(only, config_model))),
        [only] => Ok(StartupPlan::FirstListed(only.name.clone())),
        _ => Err(ModelRefError::NoModel {
            known: names(endpoints),
        }),
    }
}

/// The config model when the catalog places it on `endpoint`'s provider,
/// else the catalog default for that provider.
fn builtin_default(endpoint: &Endpoint, config_model: &str) -> EndpointRef {
    let model = if !config_model.is_empty()
        && WireDialect::of_catalog_model(config_model) == Some(endpoint.dialect)
    {
        config_model.to_string()
    } else {
        meerkat_models::default_model(endpoint.provider())
            .unwrap_or_default()
            .to_string()
    };
    EndpointRef::new(endpoint.name.clone(), model)
}

/// The endpoint a session runs on, derived from its persisted provider and
/// auth binding: a binding into [`ENDPOINT_REALM`] or [`SELF_HOSTED_REALM`]
/// names a configured endpoint; no binding means default resolution, the provider's built-in
/// endpoint. `None` when that endpoint is not configured in this process (a
/// session created by another surface, or an endpoint since removed).
#[must_use]
pub fn endpoint_of(
    provider: Provider,
    auth_binding: Option<&AuthBindingRef>,
    endpoints: &EndpointRegistry,
) -> Option<EndpointName> {
    let name = match auth_binding {
        Some(binding) if is_endpoint_realm(binding.realm.as_str()) => binding.binding.as_str(),
        Some(_) => return None,
        None => WireDialect::from_provider(provider)?.as_str(),
    };
    endpoints
        .get(name)
        .map(|e| EndpointName::new(e.name.clone()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Catalogued ids (the tests pin them: inference is catalog-exact).
    const ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";
    const OPENAI_MODEL: &str = "gpt-5.5";
    const GEMINI_MODEL: &str = "gemini-3.1-pro-preview";

    fn ep(name: &str, dialect: WireDialect, builtin: bool) -> Endpoint {
        Endpoint {
            name: name.to_string(),
            dialect,
            base_url: None,
            api_key: SecretString::new("k"),
            key_env: None,
            builtin,
            priority: 0,
        }
    }

    fn r(endpoint: &str, model: &str) -> EndpointRef {
        EndpointRef::new(endpoint, model)
    }

    #[test]
    fn dialects_come_from_the_catalog_never_from_a_prefix() {
        assert_eq!(
            WireDialect::of_catalog_model(ANTHROPIC_MODEL),
            Some(WireDialect::Anthropic)
        );
        assert_eq!(
            WireDialect::of_catalog_model(OPENAI_MODEL),
            Some(WireDialect::OpenAi)
        );
        assert_eq!(
            WireDialect::of_catalog_model(GEMINI_MODEL),
            Some(WireDialect::Gemini)
        );
        // Prefixes that used to decide no longer do.
        for unknown in [
            "claude-not-catalogued",
            "gpt-not-catalogued",
            "o9",
            "gemini-x",
        ] {
            assert_eq!(WireDialect::of_catalog_model(unknown), None, "{unknown}");
        }
    }

    #[test]
    fn parse_model_ref_table() {
        let builtins = vec![
            ep("anthropic", WireDialect::Anthropic, true),
            ep("openai", WireDialect::OpenAi, true),
            ep("litellm", WireDialect::OpenAi, false),
            ep("vertex", WireDialect::Gemini, false),
        ];
        let two_openai = vec![
            ep("litellm", WireDialect::OpenAi, false),
            ep("proxy", WireDialect::OpenAi, false),
        ];
        let single = vec![ep("litellm", WireDialect::OpenAi, false)];
        type Case<'a> = (&'a str, &'a [Endpoint], Result<EndpointRef, &'a str>);
        let cases: Vec<Case<'_>> = vec![
            // explicit
            ("litellm:gpt-x", &builtins, Ok(r("litellm", "gpt-x"))),
            ("anthropic:gpt-x", &builtins, Ok(r("anthropic", "gpt-x"))),
            // model ids containing ':' after an explicit endpoint
            (
                "litellm:llama3:8b",
                &builtins,
                Ok(r("litellm", "llama3:8b")),
            ),
            // bare catalogued: builtin of its dialect wins over configured ones
            (OPENAI_MODEL, &builtins, Ok(r("openai", OPENAI_MODEL))),
            (
                ANTHROPIC_MODEL,
                &builtins,
                Ok(r("anthropic", ANTHROPIC_MODEL)),
            ),
            // bare catalogued: the only endpoint of the dialect
            (GEMINI_MODEL, &builtins, Ok(r("vertex", GEMINI_MODEL))),
            // bare: the only configured endpoint
            ("llama3:8b", &single, Ok(r("litellm", "llama3:8b"))),
            (ANTHROPIC_MODEL, &single, Ok(r("litellm", ANTHROPIC_MODEL))),
            // bare, unlisted, outside the catalog: no prefix guess
            (
                "gpt-not-catalogued",
                &builtins,
                Err("cannot tell which endpoint"),
            ),
            (
                "claude-not-catalogued",
                &builtins,
                Err("cannot tell which endpoint"),
            ),
            // ambiguous
            (OPENAI_MODEL, &two_openai, Err("cannot tell which endpoint")),
            ("mystery", &builtins, Err("cannot tell which endpoint")),
            ("nope:gpt-x", &two_openai, Err("unknown endpoint `nope`")),
            ("", &builtins, Err("empty")),
            ("litellm:", &builtins, Err("empty")),
            (OPENAI_MODEL, &[], Err("no LLM endpoint")),
        ];
        for (input, endpoints, expected) in cases {
            let got = parse_model_ref(input, endpoints, &ModelIndex::default(), None);
            match expected {
                Ok(want) => assert_eq!(got.as_ref(), Ok(&want), "{input}"),
                Err(needle) => {
                    let err = got.expect_err(input).to_string();
                    assert!(err.contains(needle), "{input}: {err}");
                }
            }
        }
        let err = parse_model_ref(OPENAI_MODEL, &two_openai, &ModelIndex::default(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("litellm, proxy"), "{err}");
    }

    #[test]
    fn bare_ids_resolve_by_lookup_with_tie_breaks() {
        let mut high = ep("proxy", WireDialect::OpenAi, false);
        high.priority = 5;
        let endpoints = vec![
            ep("anthropic", WireDialect::Anthropic, true),
            ep("openai", WireDialect::OpenAi, true),
            ep("litellm", WireDialect::OpenAi, false),
            high,
        ];
        let mut index = ModelIndex::default();
        index.insert(
            "litellm",
            ["only-on-litellm", OPENAI_MODEL, "shared", ANTHROPIC_MODEL],
        );
        index.insert("openai", [OPENAI_MODEL]);
        index.insert("proxy", ["shared", OPENAI_MODEL, ANTHROPIC_MODEL]);
        index.insert("anthropic", [ANTHROPIC_MODEL]);
        type Case<'a> = (&'a str, Option<&'a str>, Result<EndpointRef, &'a str>);
        let cases: Vec<Case<'_>> = vec![
            // unique: listed by one endpoint, whatever the catalog says
            ("only-on-litellm", None, Ok(r("litellm", "only-on-litellm"))),
            // several: the current endpoint first
            (
                OPENAI_MODEL,
                Some("litellm"),
                Ok(r("litellm", OPENAI_MODEL)),
            ),
            (
                ANTHROPIC_MODEL,
                Some("proxy"),
                Ok(r("proxy", ANTHROPIC_MODEL)),
            ),
            // then the builtin of the catalogued id's dialect
            (
                OPENAI_MODEL,
                Some("anthropic"),
                Ok(r("openai", OPENAI_MODEL)),
            ),
            (ANTHROPIC_MODEL, None, Ok(r("anthropic", ANTHROPIC_MODEL))),
            // then the highest priority
            ("shared", None, Ok(r("proxy", "shared"))),
            // unlisted catalogued id → its provider's built-in endpoint
            ("gpt-5.4", None, Ok(r("openai", "gpt-5.4"))),
            // unlisted, outside the catalog, several endpoints → error
            (
                "gpt-not-catalogued",
                None,
                Err("cannot tell which endpoint"),
            ),
            ("mystery", None, Err("cannot tell which endpoint")),
            // explicit override
            ("litellm:gpt-6", None, Ok(r("litellm", "gpt-6"))),
        ];
        for (input, current, expected) in cases {
            let got = parse_model_ref(input, &endpoints, &index, current);
            match expected {
                Ok(want) => assert_eq!(got.as_ref(), Ok(&want), "{input}"),
                Err(needle) => {
                    let err = got.expect_err(input).to_string();
                    assert!(err.contains(needle), "{input}: {err}");
                }
            }
        }
        // Equal priorities: the first configured wins.
        let tied = vec![
            ep("a", WireDialect::OpenAi, false),
            ep("b", WireDialect::OpenAi, false),
        ];
        let mut index = ModelIndex::default();
        index.insert("b", ["m"]);
        index.insert("a", ["m"]);
        assert_eq!(parse_model_ref("m", &tied, &index, None), Ok(r("a", "m")));
        // Unlisted, unknown, single endpoint → that endpoint.
        let single = vec![ep("litellm", WireDialect::OpenAi, false)];
        assert_eq!(
            parse_model_ref("mystery", &single, &ModelIndex::default(), None),
            Ok(r("litellm", "mystery"))
        );
        assert!(needs_lookup(OPENAI_MODEL, &endpoints));
        assert!(!needs_lookup("litellm:gpt-x", &endpoints));
        assert!(!needs_lookup(OPENAI_MODEL, &single));
    }

    #[test]
    fn endpoint_ref_display() {
        assert_eq!(r("litellm", "gpt-x").to_string(), "litellm:gpt-x");
    }

    #[test]
    fn secret_never_debugs() {
        let mut e = ep("x", WireDialect::OpenAi, false);
        e.api_key = SecretString::new("sk-very-secret");
        assert!(!format!("{e:?}").contains("sk-very-secret"));
        assert!(!format!("{:?}", e.credentials()).contains("sk-very-secret"));
    }

    fn layered(section: EndpointSection, origin: RuleOrigin) -> LayeredEndpoint {
        LayeredEndpoint { section, origin }
    }

    fn section(dialect: Option<&str>, key_env: Option<&str>, key: Option<&str>) -> EndpointSection {
        EndpointSection {
            dialect: dialect.map(str::to_string),
            base_url: Some("http://localhost:4000".to_string()),
            api_key_env: key_env.map(str::to_string),
            api_key: key.map(str::to_string),
            priority: None,
        }
    }

    #[test]
    fn discover_validates_configured_endpoints() {
        let config = Config::default();
        let configured: BTreeMap<String, LayeredEndpoint> = [
            (
                "litellm",
                layered(
                    section(Some("openai"), Some("LITELLM_KEY"), None),
                    RuleOrigin::Realm,
                ),
            ),
            (
                "baddialect",
                layered(
                    section(Some("cohere"), Some("LITELLM_KEY"), None),
                    RuleOrigin::Realm,
                ),
            ),
            (
                "nodialect",
                layered(section(None, Some("LITELLM_KEY"), None), RuleOrigin::Realm),
            ),
            (
                "nokey",
                layered(section(Some("openai"), None, None), RuleOrigin::Realm),
            ),
            (
                "unsetenv",
                layered(
                    section(Some("openai"), Some("UNSET_VAR"), None),
                    RuleOrigin::Realm,
                ),
            ),
            (
                "openai",
                layered(
                    section(Some("openai"), Some("LITELLM_KEY"), None),
                    RuleOrigin::Realm,
                ),
            ),
            (
                "projenv",
                layered(
                    section(Some("openai"), Some("LITELLM_KEY"), None),
                    RuleOrigin::Project,
                ),
            ),
            (
                "bad.name",
                layered(
                    section(Some("openai"), Some("LITELLM_KEY"), None),
                    RuleOrigin::Realm,
                ),
            ),
        ]
        .into_iter()
        .map(|(n, l)| (n.to_string(), l))
        .collect();
        let (registry, notices) =
            EndpointRegistry::discover_with(&config, None, &configured, |var| {
                (var == "LITELLM_KEY").then(|| "sk-litellm".to_string())
            })
            .unwrap();
        let litellm = registry.get("litellm").expect("litellm");
        assert_eq!(litellm.dialect, WireDialect::OpenAi);
        assert_eq!(litellm.api_key.expose(), "sk-litellm");
        assert!(!litellm.builtin);
        for skipped in [
            "baddialect",
            "nodialect",
            "nokey",
            "unsetenv",
            "projenv",
            "bad.name",
        ] {
            assert!(registry.get(skipped).is_none(), "{skipped}");
            assert!(
                notices
                    .iter()
                    .any(|n| matches!(n, Notice::EndpointSkipped { name, .. } if name == skipped)),
                "{skipped}: {notices:?}"
            );
        }
        // The configured `openai` clashes with the built-in name.
        assert!(registry.iter().all(|e| e.name != "openai" || e.builtin));
        assert!(
            notices
                .iter()
                .any(|n| matches!(n, Notice::EndpointSkipped { name, .. } if name == "openai"))
        );
        let text: Vec<String> = notices.iter().map(ToString::to_string).collect();
        assert!(text.iter().all(|t| !t.contains("sk-litellm")));
    }

    #[test]
    fn an_inline_api_key_is_a_typed_startup_error_naming_api_key_env() {
        let configured: BTreeMap<String, LayeredEndpoint> = [
            (
                "litellm".to_string(),
                layered(
                    section(Some("openai"), Some("LITELLM_KEY"), None),
                    RuleOrigin::Global,
                ),
            ),
            (
                "plain".to_string(),
                layered(
                    section(Some("anthropic"), None, Some("sk-plain-secret")),
                    RuleOrigin::Global,
                ),
            ),
        ]
        .into_iter()
        .collect();
        let err = EndpointRegistry::discover_with(&Config::default(), None, &configured, |_| {
            Some("sk-litellm".to_string())
        })
        .unwrap_err();
        assert!(
            matches!(&err, EndpointError::InlineApiKey { name } if name == "plain"),
            "{err}"
        );
        let text = err.to_string();
        assert!(text.contains("api_key_env"), "{text}");
        assert!(text.contains("[endpoints.plain]"), "{text}");
        assert!(!text.contains("sk-plain-secret"), "{text}");
        // A start-up error: exit 2.
        assert_eq!(crate::TuiError::from(err).exit_code(), 2);
    }

    fn plan_startup_t(
        request: StartupRequest<'_>,
        endpoints: &[Endpoint],
    ) -> Result<StartupPlan, ModelRefError> {
        plan_startup(request, endpoints, &ModelIndex::default())
    }

    #[test]
    fn startup_plans() {
        let both = vec![
            ep("anthropic", WireDialect::Anthropic, true),
            ep("litellm", WireDialect::OpenAi, false),
        ];
        let only_litellm = vec![ep("litellm", WireDialect::OpenAi, false)];
        let only_openai = vec![ep("openai", WireDialect::OpenAi, true)];
        let req = |model, endpoint, config_model| StartupRequest {
            model,
            endpoint,
            config_model,
        };
        let resolved = |e: &str, m: &str| Ok(StartupPlan::Resolved(r(e, m)));
        assert_eq!(
            plan_startup_t(req(Some("litellm:gpt-x"), None, ""), &both),
            resolved("litellm", "gpt-x")
        );
        assert_eq!(
            plan_startup_t(req(Some("gpt-x"), Some("litellm"), ""), &both),
            resolved("litellm", "gpt-x")
        );
        assert_eq!(
            plan_startup_t(req(Some(ANTHROPIC_MODEL), None, ""), &both),
            resolved("anthropic", ANTHROPIC_MODEL)
        );
        assert_eq!(
            plan_startup_t(req(None, Some("litellm"), ANTHROPIC_MODEL), &both),
            Ok(StartupPlan::FirstListed("litellm".into()))
        );
        assert_eq!(
            plan_startup_t(req(None, None, ANTHROPIC_MODEL), &both),
            resolved("anthropic", ANTHROPIC_MODEL)
        );
        // Config model of a dialect nobody serves, one endpoint: list it.
        assert_eq!(
            plan_startup_t(req(None, None, ANTHROPIC_MODEL), &only_litellm),
            Ok(StartupPlan::FirstListed("litellm".into()))
        );
        // ... or use the catalog default of a built-in.
        let Ok(StartupPlan::Resolved(choice)) =
            plan_startup_t(req(None, None, ANTHROPIC_MODEL), &only_openai)
        else {
            panic!("expected a resolved model");
        };
        assert_eq!(choice.endpoint, "openai");
        assert_eq!(
            Some(choice.model.as_str()),
            meerkat_models::default_model(Provider::OpenAI)
        );
        let err = plan_startup_t(req(None, Some("nope"), ""), &both).unwrap_err();
        assert!(err.to_string().contains("anthropic, litellm"), "{err}");
        let err = plan_startup_t(
            req(None, None, "mystery"),
            &[
                ep("litellm", WireDialect::OpenAi, false),
                ep("proxy", WireDialect::OpenAi, false),
            ],
        )
        .unwrap_err();
        assert!(matches!(err, ModelRefError::NoModel { .. }), "{err}");
        assert_eq!(
            plan_startup_t(req(None, None, ""), &[]),
            Err(ModelRefError::NoEndpoints)
        );
    }

    #[test]
    fn a_sessions_endpoint_is_derived_from_its_binding_and_provider() {
        let registry = EndpointRegistry::from_endpoints(vec![
            ep("anthropic", WireDialect::Anthropic, true),
            ep("litellm", WireDialect::OpenAi, false),
        ]);
        let litellm = registry.get("litellm").unwrap().binding();
        assert_eq!(
            endpoint_of(Provider::OpenAI, litellm.as_ref(), &registry),
            Some(EndpointName::new("litellm"))
        );
        assert_eq!(
            endpoint_of(Provider::Anthropic, None, &registry),
            Some(EndpointName::new("anthropic"))
        );
        // No built-in openai endpoint in this process, and a binding of
        // another realm names no TUI endpoint.
        assert_eq!(endpoint_of(Provider::OpenAI, None, &registry), None);
        let foreign = AuthBindingRef {
            realm: RealmId::parse("elsewhere").unwrap(),
            binding: BindingId::parse("litellm").unwrap(),
            profile: None,
            origin: BindingOrigin::Configured,
        };
        assert_eq!(
            endpoint_of(Provider::OpenAI, Some(&foreign), &registry),
            None
        );
        // A removed endpoint is no endpoint.
        let gone = ep("gone", WireDialect::OpenAi, false).binding();
        assert_eq!(
            endpoint_of(Provider::OpenAI, gone.as_ref(), &registry),
            None
        );
    }

    fn configured(name: &str, dialect: WireDialect, key_env: Option<&str>) -> Endpoint {
        Endpoint {
            name: name.to_string(),
            dialect,
            base_url: Some(format!("http://127.0.0.1:4000/{name}")),
            api_key: SecretString::new("sk-inline"),
            key_env: key_env.map(str::to_string),
            builtin: false,
            priority: 0,
        }
    }

    #[test]
    fn configured_endpoints_become_bindings_off_every_chain() {
        let endpoints = vec![
            ep("openai", WireDialect::OpenAi, true),
            configured("litellm", WireDialect::OpenAi, Some("LITELLM_KEY")),
            configured("gateway", WireDialect::Anthropic, Some("GATEWAY_KEY")),
            configured("vertex", WireDialect::Gemini, Some("VERTEX_KEY")),
        ];
        let mut config = Config::default();
        let before = meerkat_core::connection::resolve_auth_binding_or_default_for_provider(
            &config,
            Provider::OpenAI,
            None,
            None,
            true,
        )
        .unwrap();
        config.realm.insert(
            ENDPOINT_REALM.to_string(),
            endpoint_section(&endpoints).unwrap(),
        );
        config.realm.insert(
            SELF_HOSTED_REALM.to_string(),
            self_hosted_section(&endpoints).unwrap(),
        );
        assert!(
            endpoints[0].binding().is_none(),
            "built-ins resolve by default"
        );
        for endpoint in &endpoints[1..] {
            let binding = endpoint.binding().unwrap();
            let target =
                meerkat_core::connection::resolve_explicit_auth_binding_target(&config, &binding)
                    .unwrap();
            // An OpenAI-dialect proxy is the engine's self-hosted server.
            assert_eq!(
                target.backend.provider,
                endpoint.engine_provider(),
                "{}",
                endpoint.name
            );
            assert_eq!(
                target.backend.server,
                endpoint.self_hosted_server(),
                "{}",
                endpoint.name
            );
            assert_eq!(
                target.backend.base_url, endpoint.base_url,
                "{}",
                endpoint.name
            );
            let expected = CredentialSourceSpec::Env {
                env: endpoint.key_env.clone().unwrap(),
                fallback: Vec::new(),
            };
            assert!(target.auth_profile.source == expected, "{}", endpoint.name);
        }
        // The built-in provider's default resolution is untouched.
        let after = meerkat_core::connection::resolve_auth_binding_or_default_for_provider(
            &config,
            Provider::OpenAI,
            None,
            None,
            true,
        )
        .unwrap();
        assert_eq!(before.auth_binding, after.auth_binding);
        assert_eq!(before.backend.base_url, after.backend.base_url);
        // Only built-ins: nothing to project.
        assert!(
            endpoint_section(&endpoints[..1])
                .unwrap()
                .binding
                .is_empty()
        );
    }

    /// The config runtime of the document at `path`, its sidecar beside it.
    fn config_runtime(path: &std::path::Path) -> ConfigRuntime {
        ConfigRuntime::new(
            std::sync::Arc::new(meerkat_core::FileConfigStore::new(
                path.to_path_buf(),
                meerkat_models::canonical(),
            )),
            path.with_file_name("config_state.json"),
        )
    }

    /// A writer that commits between the read and the write makes the
    /// write conflict on the generation; the patch is rebuilt from the new
    /// document and written then.
    #[tokio::test]
    async fn a_write_that_lost_the_generation_race_is_rebuilt_and_retried() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let runtime = config_runtime(&path);
        let sidecar = path.with_file_name("config_state.json");
        let reads = std::sync::atomic::AtomicU32::new(0);
        let wrote = patch_document(&runtime, |persisted| {
            if reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                // Another surface commits a write of its own meanwhile.
                std::fs::write(&path, "[models.\"theirs\"]\nprovider = \"openai\"\n").unwrap();
                std::fs::write(&sidecar, r#"{"generation": 7}"#).unwrap();
            }
            assert_eq!(
                persisted.models.custom.contains_key("theirs"),
                reads.load(std::sync::atomic::Ordering::SeqCst) > 1
            );
            Ok(Some(ConfigDelta(serde_json::json!({
                "models": { "ours": { "provider": "anthropic" } }
            }))))
        })
        .await
        .unwrap();
        assert!(wrote);
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2);
        let snapshot = runtime.get().await.unwrap();
        assert_eq!(snapshot.generation, 8);
        assert!(snapshot.config.models.custom.contains_key("theirs"));
        assert!(snapshot.config.models.custom.contains_key("ours"));
    }

    /// rkat-tui's write waits for the writer lock another surface holds.
    #[tokio::test]
    async fn a_write_waits_for_the_writer_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let runtime = config_runtime(&path);
        let held = meerkat_core::config_runtime::ConfigWriterLock::acquire(
            &path.with_file_name("config_state.lock"),
        )
        .await
        .unwrap();
        let owned = OwnedProfiles::in_realm(tmp.path());
        let write = ensure_model_profile(&runtime, &owned, Provider::OpenAI, "llama3:8b", None);
        tokio::pin!(write);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut write)
                .await
                .is_err(),
            "the write went past a held writer lock"
        );
        assert!(!path.exists());
        drop(held);
        assert!(write.await.unwrap());
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("llama3:8b")
        );
    }

    #[tokio::test]
    async fn endpoints_are_projected_into_the_realm_document_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let store = config_runtime(&path);
        let endpoints = vec![
            ep("anthropic", WireDialect::Anthropic, true),
            configured("litellm", WireDialect::OpenAi, Some("LITELLM_KEY")),
        ];
        assert!(project_endpoints(&store, &endpoints).await.unwrap());
        // Unchanged: nothing is written again.
        assert!(!project_endpoints(&store, &endpoints).await.unwrap());
        // What another surface loading the realm document reads resolves
        // the session's binding.
        let reread = meerkat_core::FileConfigStore::new(path.clone(), meerkat_models::canonical());
        let reread = meerkat_core::ConfigStore::get(&reread).await.unwrap();
        let binding = endpoints[1].binding().unwrap();
        let target =
            meerkat_core::connection::resolve_explicit_auth_binding_target(&reread, &binding)
                .unwrap();
        assert_eq!(target.backend.base_url, endpoints[1].base_url);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("LITELLM_KEY"), "{text}");
        assert!(!text.contains("sk-inline"), "{text}");
        // The document records the variable's name only: no secret, and no
        // inline-secret source.
        assert!(!text.contains("secret"), "{text}");
        // A configured endpoint without a variable is refused, and the
        // document is left as it was.
        let inline = vec![configured("gateway", WireDialect::Anthropic, None)];
        assert!(matches!(
            project_endpoints(&store, &inline).await,
            Err(EndpointError::InlineApiKey { name }) if name == "gateway"
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        let gateway = vec![configured(
            "gateway",
            WireDialect::Anthropic,
            Some("GATEWAY_KEY"),
        )];
        assert!(project_endpoints(&store, &gateway).await.unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("GATEWAY_KEY"), "{text}");
        assert!(!text.contains("sk-inline"), "{text}");
        // Earlier endpoints stay, so their sessions keep resolving.
        let reread = store.get().await.unwrap();
        // Each write was one generation of the realm's config runtime.
        assert_eq!(reread.generation, 2);
        assert!(
            reread.config.realm[SELF_HOSTED_REALM]
                .binding
                .contains_key("litellm")
        );
        assert!(
            reread.config.realm[ENDPOINT_REALM]
                .binding
                .contains_key("gateway")
        );
    }

    /// An OpenAI-dialect proxy's models are self-hosted aliases with the
    /// listing's facts: a catalogued id is no collision (no `[models]`
    /// entry), the first alias becomes the default the engine needs once
    /// there are two, a later listing refreshes an alias, and none keeps
    /// it as it was; the endpoint's server is projected with its
    /// connection.
    #[tokio::test]
    async fn a_proxy_s_models_become_aliases_with_its_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let store = config_runtime(&tmp.path().join("config.toml"));
        let owned = OwnedProfiles::in_realm(tmp.path());
        let proxy = configured("litellm", WireDialect::OpenAi, Some("LITELLM_KEY"));
        assert!(
            project_endpoints(&store, std::slice::from_ref(&proxy))
                .await
                .unwrap()
        );
        let listed = |id: &str, window: u64| ModelEntry {
            id: id.into(),
            display_name: None,
            source: crate::models::ModelSource::Provider,
            context_window: Some(window),
            max_output_tokens: Some(4_096),
            capabilities: crate::models::ModelCapabilities {
                reasoning: Some(true),
                vision: Some(true),
                function_calling: Some(true),
            },
        };
        let opus = listed(ANTHROPIC_MODEL, 50_000);
        let local = listed("llama3:8b", 8_192);
        assert!(
            ensure_aliases(
                &store,
                &proxy,
                &[(ANTHROPIC_MODEL, Some(&opus)), ("llama3:8b", Some(&local))]
            )
            .await
            .unwrap()
        );
        let config = store.get().await.unwrap().config;
        assert!(config.models.custom.is_empty());
        assert_eq!(
            config.self_hosted.servers["litellm"].base_url,
            "http://127.0.0.1:4000/litellm"
        );
        let alias = format!("litellm:{ANTHROPIC_MODEL}");
        let entry = &config.self_hosted.models[&alias];
        assert_eq!(entry.server, "litellm");
        assert_eq!(entry.remote_model, ANTHROPIC_MODEL);
        assert_eq!(entry.context_window, Some(50_000));
        assert!(entry.supports_reasoning && entry.supports_thinking && entry.vision);
        assert!(entry.supports_temperature);
        assert_eq!(
            config.self_hosted.default_model.as_deref(),
            Some(alias.as_str())
        );
        let registry = config.model_registry(meerkat_models::canonical()).unwrap();
        assert_eq!(
            registry_context_window(&config, Provider::SelfHosted, "litellm:llama3:8b"),
            Some(8_192)
        );
        assert_eq!(
            registry
                .entry_for_provider(Provider::SelfHosted, &alias)
                .map(|e| e.id.clone()),
            Some(alias.clone())
        );
        // No listing now: the alias stays as it was; the same listing
        // writes nothing; a new window refreshes it.
        assert!(
            !ensure_aliases(&store, &proxy, &[(ANTHROPIC_MODEL, None)])
                .await
                .unwrap()
        );
        assert!(
            !ensure_aliases(&store, &proxy, &[(ANTHROPIC_MODEL, Some(&opus))])
                .await
                .unwrap()
        );
        let bigger = listed(ANTHROPIC_MODEL, 60_000);
        assert!(
            ensure_endpoint_model(&store, &owned, &proxy, ANTHROPIC_MODEL, Some(&bigger))
                .await
                .unwrap()
        );
        let config = store.get().await.unwrap().config;
        assert_eq!(
            config.self_hosted.models[&alias].context_window,
            Some(60_000)
        );
        // A built-in or another dialect's endpoint writes no alias.
        let gateway = configured("gateway", WireDialect::Anthropic, Some("GATEWAY_KEY"));
        assert!(
            !ensure_aliases(&store, &gateway, &[("x", None)])
                .await
                .unwrap()
        );
    }

    #[test]
    fn an_engine_alias_is_the_model_on_its_endpoint() {
        assert_eq!(
            from_engine("litellm:llama3:8b", "self_hosted"),
            ("llama3:8b".to_string(), "openai".to_string())
        );
        assert_eq!(
            from_engine(ANTHROPIC_MODEL, "anthropic"),
            (ANTHROPIC_MODEL.to_string(), "anthropic".to_string())
        );
        let proxy = configured("litellm", WireDialect::OpenAi, Some("K"));
        assert_eq!(proxy.engine_model("gpt-5.5"), "litellm:gpt-5.5");
        assert_eq!(proxy.engine_provider(), Provider::SelfHosted);
        let gateway = configured("gateway", WireDialect::Anthropic, Some("K"));
        assert_eq!(gateway.engine_model(ANTHROPIC_MODEL), ANTHROPIC_MODEL);
        assert_eq!(gateway.engine_provider(), Provider::Anthropic);
        assert_eq!(gateway.self_hosted_server(), None);
        let builtin = ep("openai", WireDialect::OpenAi, true);
        assert!(!builtin.is_self_hosted());
    }

    #[tokio::test]
    async fn a_configured_endpoint_builds_its_dialect_client() {
        let tmp = tempfile::tempdir().unwrap();
        let factory = meerkat::AgentFactory::new(tmp.path().to_path_buf());
        for endpoint in [
            // `PATH` is set wherever tests run; its value only has to be
            // a non-empty key for the client to build.
            configured("litellm", WireDialect::OpenAi, Some("PATH")),
            configured("gateway", WireDialect::Anthropic, Some("PATH")),
            configured("vertex", WireDialect::Gemini, Some("PATH")),
        ] {
            let mut config = Config::default();
            let projection = EndpointProjection::of(std::slice::from_ref(&endpoint)).unwrap();
            config
                .realm
                .insert(ENDPOINT_REALM.to_string(), projection.section);
            config
                .realm
                .insert(SELF_HOSTED_REALM.to_string(), projection.self_hosted);
            config.self_hosted.servers = projection.servers;
            let model = "served-only-here";
            // An OpenAI-dialect proxy's model is its self-hosted alias; the
            // others get a `[models.<id>]` profile.
            if endpoint.is_self_hosted() {
                config.self_hosted.models.insert(
                    endpoint.engine_model(model),
                    alias_entry(&endpoint, model, None),
                );
            } else {
                let entry = model_profile_entry(&config, endpoint.provider(), model, None)
                    .unwrap()
                    .unwrap();
                config.models.custom.insert(model.to_string(), entry);
            }
            let identity = meerkat_core::SessionLlmIdentity {
                model: endpoint.engine_model(model),
                provider: endpoint.engine_provider(),
                self_hosted_server_id: endpoint.self_hosted_server(),
                provider_params: None,
                auth_binding: endpoint.binding(),
            };
            let client = factory
                .build_llm_client_for_identity(&config, &identity)
                .await
                .unwrap_or_else(|err| panic!("{}: {err}", endpoint.name));
            assert_eq!(
                client.provider(),
                endpoint.engine_provider(),
                "{}",
                endpoint.name
            );
        }
    }

    /// A catalog addition never stops the realm: a `[models.<id>]` entry
    /// rkat-tui wrote (recorded in its realm state) for an id the catalog
    /// now defines is dropped before any write, with a notice; the same
    /// entry the user wrote is a typed error naming it, and the document
    /// is left alone; an entry for an uncatalogued id is kept.
    #[tokio::test]
    async fn profiles_the_catalog_now_defines_are_pruned_or_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        // A catalogued id stands in for a future catalog addition: the
        // document is written as an earlier release would have.
        let shadowing = format!(
            "[models.\"{ANTHROPIC_MODEL}\"]\nprovider = \"anthropic\"\ncontext_window = 50000\n\n[models.\"proxy-alias\"]\nprovider = \"anthropic\"\n"
        );
        std::fs::write(&path, &shadowing).unwrap();
        let store = config_runtime(&path);
        let owned = OwnedProfiles::in_realm(tmp.path());

        // The user's entry: a typed error, nothing written.
        let err = prune_catalogued_profiles(&store, &owned).await.unwrap_err();
        assert!(
            matches!(&err, EndpointError::ProfileShadowsCatalog { model } if model == ANTHROPIC_MODEL),
            "{err}"
        );
        assert!(
            err.to_string()
                .contains(&format!("[models.\"{ANTHROPIC_MODEL}\"]"))
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), shadowing);

        // rkat-tui's own: dropped, the record forgets it, the alias stays.
        owned.add(ANTHROPIC_MODEL).unwrap();
        owned.add("proxy-alias").unwrap();
        let dropped = prune_catalogued_profiles(&store, &owned).await.unwrap();
        assert_eq!(dropped, [ANTHROPIC_MODEL]);
        let config = store.get().await.unwrap().config;
        assert!(!config.models.custom.contains_key(ANTHROPIC_MODEL));
        assert!(config.models.custom.contains_key("proxy-alias"));
        assert!(config.model_registry(meerkat_models::canonical()).is_ok());
        assert_eq!(
            owned.load().unwrap(),
            std::collections::BTreeSet::from(["proxy-alias".to_string()])
        );
        let notice = pruned_notice(&dropped).unwrap();
        assert!(
            notice.contains(&format!("[models.\"{ANTHROPIC_MODEL}\"]")),
            "{notice}"
        );
        assert_eq!(pruned_notice(&[]), None);
        // Nothing left to drop.
        assert!(
            prune_catalogued_profiles(&store, &owned)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// The profiles rkat-tui writes are recorded as its own; a profile the
    /// user wrote that it only refreshes is not.
    #[tokio::test]
    async fn written_profiles_are_recorded_as_rkat_tui_s() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[models.\"theirs\"]\nprovider = \"openai\"\n").unwrap();
        let store = config_runtime(&path);
        let owned = OwnedProfiles::in_realm(tmp.path());
        let listed = |id: &str| ModelEntry {
            id: id.into(),
            display_name: None,
            source: crate::models::ModelSource::Provider,
            context_window: Some(8_192),
            max_output_tokens: None,
            capabilities: crate::models::ModelCapabilities::default(),
        };
        for model in ["mine", "theirs"] {
            assert!(
                ensure_model_profile(
                    &store,
                    &owned,
                    Provider::OpenAI,
                    model,
                    Some(&listed(model))
                )
                .await
                .unwrap(),
                "{model}"
            );
        }
        assert_eq!(
            owned.load().unwrap(),
            std::collections::BTreeSet::from(["mine".to_string()])
        );
        // An unreadable record is an error naming it.
        std::fs::write(tmp.path().join(OWNED_PROFILES_FILE), "not json").unwrap();
        let err = owned.load().unwrap_err();
        assert!(err.to_string().contains(OWNED_PROFILES_FILE), "{err}");
    }

    #[tokio::test]
    async fn profiles_are_written_for_unknown_models_and_refused_across_dialects() {
        let tmp = tempfile::tempdir().unwrap();
        let store = config_runtime(&tmp.path().join("config.toml"));
        let owned = OwnedProfiles::in_realm(tmp.path());
        assert!(
            !ensure_model_profile(&store, &owned, Provider::Anthropic, ANTHROPIC_MODEL, None)
                .await
                .unwrap()
        );
        let listed = ModelEntry {
            id: "llama3:8b".into(),
            display_name: Some("Llama 3".into()),
            source: crate::models::ModelSource::Provider,
            context_window: Some(8_192),
            max_output_tokens: Some(2_048),
            capabilities: Default::default(),
        };
        assert!(
            ensure_model_profile(&store, &owned, Provider::OpenAI, "llama3:8b", Some(&listed))
                .await
                .unwrap()
        );
        assert!(
            !ensure_model_profile(&store, &owned, Provider::OpenAI, "llama3:8b", None)
                .await
                .unwrap()
        );
        // A later listing with a larger window refreshes the profile.
        let bigger = ModelEntry {
            context_window: Some(16_384),
            ..listed.clone()
        };
        assert!(
            ensure_model_profile(&store, &owned, Provider::OpenAI, "llama3:8b", Some(&bigger))
                .await
                .unwrap()
        );
        let config = store.get().await.unwrap().config;
        let custom = &config.models.custom["llama3:8b"];
        assert_eq!(custom.provider, Provider::OpenAI);
        assert_eq!(custom.context_window, Some(16_384));
        assert_eq!(custom.max_output_tokens, Some(2_048));
        assert_eq!(
            registry_context_window(&config, Provider::OpenAI, "llama3:8b"),
            Some(16_384)
        );
        assert!(registry_context_window(&config, Provider::OpenAI, "unknown").is_none());
        // The same id on an endpoint of another dialect is refused, as is a
        // catalogued id claimed for another provider.
        let err = ensure_model_profile(&store, &owned, Provider::Anthropic, "llama3:8b", None)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                EndpointError::DialectConflict {
                    existing: Provider::OpenAI,
                    requested: Provider::Anthropic,
                    ..
                }
            ),
            "{err}"
        );
        let err = ensure_model_profile(&store, &owned, Provider::Gemini, OPENAI_MODEL, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, EndpointError::DialectConflict { .. }),
            "{err}"
        );
    }
}
