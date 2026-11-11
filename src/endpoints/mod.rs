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
//!   ([`Endpoint::is_self_hosted`]): its connection has a `self_hosted`
//!   backend naming the server,
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
//!   (the engine has no alias for them): the provider's own client talks to
//!   the endpoint's URL with its key, and a catalogued model keeps the
//!   catalog's request shaping. Only a model the engine's catalog knows runs
//!   there; any other is refused at start and at `/model`
//!   ([`EndpointError::Uncatalogued`]), naming the way out: configure the
//!   proxy as an `openai`-dialect endpoint, where every model it serves is
//!   an alias. The same holds for the built-in endpoints.
//!
//! Every configured endpoint's connection lives in one realm section,
//! [`ENDPOINT_REALM`] (an endpoint's name is unique).
//!
//! Which endpoint a session runs on is derived from its persisted identity
//! ([`endpoint_of`]): the binding names a configured endpoint, else the

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use meerkat_core::auth::{AuthConstraints, AuthMetadataDefaults};
use meerkat_core::config::{
    ConfigError, SelfHostedApiStyle, SelfHostedModelConfig, SelfHostedServerConfig,
    SelfHostedTransport,
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

mod discover;
mod project;
mod refs;
mod startup;
#[cfg(test)]
mod tests;

pub use discover::*;
pub use project::*;
pub use refs::*;
pub use startup::*;

use crate::models::ModelEntry;

/// Names of the built-in endpoints, in display order.
pub const BUILTIN_NAMES: [&str; 3] = ["anthropic", "openai", "gemini"];

/// The realm section holding one connection per configured endpoint (see
/// the module docs), written into the realm's own config document.
pub const ENDPOINT_REALM: &str = "rkat-tui-endpoints";

/// Whether `realm` is the section rkat-tui projects its configured
/// endpoints into: a binding there names a configured endpoint.
#[must_use]
pub fn is_endpoint_realm(realm: &str) -> bool {
    realm == ENDPOINT_REALM
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

/// Which of the engine's OpenAI-compatible APIs a self-hosted endpoint
/// ([`Endpoint::is_self_hosted`]) is spoken to with: `api_style` in
/// `[endpoints.<name>]` (and `--endpoint-def`'s `api_style=`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ApiStyle {
    /// The Responses API (`<base>/v1/responses`), what rkat-tui's OpenAI
    /// dialect always spoke.
    #[default]
    Responses,
    /// Chat Completions (`<base>/v1/chat/completions`), for a proxy or
    /// server without the Responses API.
    ChatCompletions,
}

impl ApiStyle {
    /// Every style, in the order the error message names them.
    pub const ALL: [Self; 2] = [Self::Responses, Self::ChatCompletions];

    /// The setting's value (`responses`, `chat_completions`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
        }
    }

    /// Parse a setting's value; anything else is refused, naming the key.
    ///
    /// # Errors
    ///
    /// [`ApiStyleError`] for any other value.
    pub fn parse(value: &str) -> Result<Self, ApiStyleError> {
        Self::ALL
            .into_iter()
            .find(|style| style.as_str() == value)
            .ok_or_else(|| ApiStyleError(value.to_string()))
    }

    /// The engine's API style.
    #[must_use]
    pub fn engine(self) -> SelfHostedApiStyle {
        match self {
            Self::Responses => SelfHostedApiStyle::Responses,
            Self::ChatCompletions => SelfHostedApiStyle::ChatCompletions,
        }
    }
}

impl fmt::Display for ApiStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An `api_style` value that is neither `responses` nor `chat_completions`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`api_style` must be \"responses\" or \"chat_completions\", not \"{0}\"")]
pub struct ApiStyleError(pub String);

impl serde::Serialize for ApiStyle {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for ApiStyle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
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
    /// Tie-break when several endpoints list the same bare model id
    /// (higher wins; built-ins are 0).
    pub priority: i64,
    /// What it is to the engine.
    kind: Kind,
}

/// What an endpoint is to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// A built-in provider endpoint: default resolution, catalog models.
    Builtin,
    /// A configured Anthropic- or Gemini-dialect gateway: its dialect's
    /// client on its URL, catalog models.
    Gateway {
        /// The environment variable the key came from (`api_key_env`),
        /// which the realm document's auth profile names.
        key_env: String,
    },
    /// A configured OpenAI-dialect endpoint: the engine's self-hosted
    /// server, every model an alias.
    SelfHosted {
        /// As for a gateway.
        key_env: String,
        /// The API it is spoken to with.
        api_style: ApiStyle,
    },
}

impl Endpoint {
    /// The built-in endpoint of `dialect` (named after it), with the key
    /// and base URL the realm or the environment gives it.
    #[must_use]
    pub fn builtin(dialect: WireDialect, base_url: Option<String>, api_key: SecretString) -> Self {
        Self {
            name: dialect.as_str().to_string(),
            dialect,
            base_url,
            api_key,
            priority: 0,
            kind: Kind::Builtin,
        }
    }

    /// A configured endpoint (`[endpoints.<name>]`) whose key `api_key`
    /// came from the environment variable `key_env`: the engine's
    /// self-hosted server spoken to with `api_style` in the `openai`
    /// dialect, a gateway in the others (where `api_style` means nothing).
    #[must_use]
    pub fn configured(
        name: impl Into<String>,
        dialect: WireDialect,
        base_url: Option<String>,
        api_key: SecretString,
        key_env: impl Into<String>,
        priority: i64,
        api_style: ApiStyle,
    ) -> Self {
        let key_env = key_env.into();
        Self {
            name: name.into(),
            dialect,
            base_url,
            api_key,
            priority,
            kind: match dialect {
                WireDialect::OpenAi => Kind::SelfHosted { key_env, api_style },
                WireDialect::Anthropic | WireDialect::Gemini => Kind::Gateway { key_env },
            },
        }
    }

    /// Whether this is one of the built-in provider endpoints.
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        self.kind == Kind::Builtin
    }

    /// The environment variable a configured endpoint's key came from,
    /// which the realm document's auth profile names (`None`: built-in).
    #[must_use]
    pub fn key_env(&self) -> Option<&str> {
        match &self.kind {
            Kind::Builtin => None,
            Kind::Gateway { key_env } | Kind::SelfHosted { key_env, .. } => Some(key_env),
        }
    }

    /// The API a self-hosted endpoint is spoken to with (the default for
    /// every other endpoint, where it means nothing).
    #[must_use]
    pub fn api_style(&self) -> ApiStyle {
        match &self.kind {
            Kind::SelfHosted { api_style, .. } => *api_style,
            Kind::Builtin | Kind::Gateway { .. } => ApiStyle::default(),
        }
    }

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
        matches!(self.kind, Kind::SelfHosted { .. })
    }

    /// The engine's model id of `model` on this endpoint: the alias
    /// `<endpoint>:<model>` on a self-hosted endpoint, else `model`.
    #[must_use]
    pub fn engine_model(&self, model: &str) -> String {
        match self.kind {
            Kind::SelfHosted { .. } => EndpointRef::new(self.name.as_str(), model).to_string(),
            Kind::Builtin | Kind::Gateway { .. } => model.to_string(),
        }
    }

    /// The engine provider of this endpoint's sessions:
    /// [`Provider::SelfHosted`] on a self-hosted endpoint, else the
    /// dialect's provider.
    #[must_use]
    pub fn engine_provider(&self) -> Provider {
        match self.kind {
            Kind::SelfHosted { .. } => Provider::SelfHosted,
            Kind::Builtin | Kind::Gateway { .. } => self.provider(),
        }
    }

    /// The engine's self-hosted server of this endpoint's sessions.
    #[must_use]
    pub fn self_hosted_server(&self) -> Option<String> {
        match self.kind {
            Kind::SelfHosted { .. } => Some(self.name.clone()),
            Kind::Builtin | Kind::Gateway { .. } => None,
        }
    }

    /// The explicit auth binding a session on this endpoint carries: `None`
    /// for a built-in endpoint (default resolution), the endpoint's
    /// connection in [`ENDPOINT_REALM`] otherwise.
    #[must_use]
    pub fn binding(&self) -> Option<AuthBindingRef> {
        if self.is_builtin() {
            return None;
        }
        Some(AuthBindingRef {
            realm: RealmId::parse(ENDPOINT_REALM).ok()?,
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
    /// A model the engine's catalog does not know, on an endpoint that is
    /// not a self-hosted (OpenAI-dialect) one.
    #[error(
        "model `{model}` is not in Meerkat's model catalog, and {where_} runs only catalogued models ({dialect} dialect); to use any model a proxy serves, configure it as an `openai`-dialect endpoint ([endpoints.<name>] dialect = \"openai\" with its OpenAI-compatible base_url), where every model it lists runs as `<name>:<model>`"
    )]
    Uncatalogued {
        /// The model.
        model: String,
        /// `the built-in endpoint `x`` or `endpoint `x``.
        where_: String,
        /// The endpoint's dialect.
        dialect: WireDialect,
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
