//! The endpoints projected into the realm's own config document: their
//! connections, the self-hosted servers and their model aliases.

use super::*;

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
            servers: self_hosted_servers(endpoints),
        })
    }
}

/// The `self_hosted.servers` entry of each self-hosted endpoint of
/// `endpoints` ([`Endpoint::is_self_hosted`]): the OpenAI-compatible
/// transport on the endpoint's URL (the public OpenAI API without one),
/// speaking the endpoint's [`ApiStyle`] ([`server_base_url`]).
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
                    base_url: server_base_url(&base_url, endpoint.api_style()),
                    api_style: endpoint.api_style().engine(),
                },
            )
        })
        .collect()
}

/// The engine server's base URL for an endpoint at `base_url` (the
/// server's root, as the manual asks, with or without `/v1`) in `style`.
/// The engine's two clients read it differently: the Responses client
/// trims a trailing `/v1` and appends `/v1/responses`, the Chat
/// Completions client appends `/chat/completions` as given. So Chat
/// Completions gets `/v1` appended when the URL does not end in it, and
/// the Responses URL is left as configured.
#[must_use]
pub fn server_base_url(base_url: &str, style: ApiStyle) -> String {
    match style {
        ApiStyle::Responses => base_url.to_string(),
        ApiStyle::ChatCompletions => {
            let root = base_url.trim_end_matches('/');
            if root.ends_with("/v1") {
                root.to_string()
            } else {
                format!("{root}/v1")
            }
        }
    }
}

/// The [`ENDPOINT_REALM`] section entries of the configured endpoints of
/// `endpoints` (built-in endpoints add nothing).
///
/// # Errors
///
/// As [`connections`].
pub fn endpoint_section(endpoints: &[Endpoint]) -> Result<RealmConfigSection, EndpointError> {
    connections(endpoints)
}

/// One connection (backend, auth profile, binding, each named after the
/// endpoint) per configured endpoint of `endpoints`.
///
/// # Errors
///
/// [`EndpointError::BindingName`] when an endpoint name is not a valid
/// binding id, [`EndpointError::NoApiKeyBackend`] when upstream names no
/// API-key backend for a dialect. A configured endpoint always names the
/// environment variable its key comes from (the document never holds a
/// key).
fn connections(endpoints: &[Endpoint]) -> Result<RealmConfigSection, EndpointError> {
    let mut section = RealmConfigSection::default();
    let configured = endpoints
        .iter()
        .filter_map(|endpoint| Some((endpoint, endpoint.key_env()?)));
    for (endpoint, env) in configured {
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
        let source = CredentialSourceSpec::Env {
            env: env.to_string(),
            fallback: Vec::new(),
        };
        section.backend.insert(
            endpoint.name.clone(),
            BackendProfileConfig {
                provider: provider.as_str().to_string(),
                backend_kind,
                base_url: if endpoint.is_self_hosted() {
                    endpoint
                        .base_url
                        .as_deref()
                        .map(|url| server_base_url(url, endpoint.api_style()))
                } else {
                    endpoint.base_url.clone()
                },
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
    for (realm, section) in [(ENDPOINT_REALM, &projection.section)] {
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
    if projection.section.binding.is_empty() {
        return Ok(false);
    }
    patch_document(runtime, |persisted| endpoint_patch(persisted, &projection)).await
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
/// alias ([`ensure_aliases`]) on a self-hosted endpoint; anywhere else the
/// model must be one the engine's model registry knows on the endpoint's
/// provider ([`check_catalogued`]). Returns whether the document changed.
///
/// # Errors
///
/// As [`ensure_aliases`] and [`check_catalogued`].
pub async fn ensure_endpoint_model(
    runtime: &ConfigRuntime,
    endpoint: &Endpoint,
    model: &str,
    listed: Option<&ModelEntry>,
) -> Result<bool, EndpointError> {
    if endpoint.is_self_hosted() {
        return ensure_aliases(runtime, endpoint, &[(model, listed)]).await;
    }
    let snapshot = runtime.get().await.map_err(EndpointError::ConfigSnapshot)?;
    check_catalogued(&snapshot.config, endpoint, model)?;
    Ok(false)
}

/// Refuse `model` on `endpoint` when it is not self-hosted and the engine's
/// model registry (the catalog, and any `[models.<id>]` entry the realm
/// config holds) does not know it on the endpoint's provider: the engine
/// would refuse the build, and rkat-tui no longer writes profiles for
/// models only an endpoint knows.
///
/// # Errors
///
/// [`EndpointError::Uncatalogued`] naming the way out,
/// [`EndpointError::Registry`] when the registry cannot be built.
pub fn check_catalogued(
    config: &Config,
    endpoint: &Endpoint,
    model: &str,
) -> Result<(), EndpointError> {
    let where_ = match endpoint.kind {
        Kind::SelfHosted { .. } => return Ok(()),
        Kind::Builtin => format!("the built-in endpoint `{}`", endpoint.name),
        Kind::Gateway { .. } => format!("endpoint `{}`", endpoint.name),
    };
    let registry = config
        .model_registry(meerkat_models::canonical())
        .map_err(EndpointError::Registry)?;
    if registry
        .profile_for_provider(endpoint.provider(), model)
        .is_some()
    {
        return Ok(());
    }
    Err(EndpointError::Uncatalogued {
        model: model.to_string(),
        where_,
        dialect: endpoint.dialect,
    })
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
