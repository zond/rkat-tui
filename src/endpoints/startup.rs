//! The start-up plan: which endpoint and model a session starts on, and
//! which endpoint a stored session runs on.

use super::*;

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
        if !endpoint.is_builtin() {
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
        [only] if only.is_builtin() => {
            Ok(StartupPlan::Resolved(builtin_default(only, config_model)))
        }
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
/// auth binding: a binding into [`ENDPOINT_REALM`] names a configured
/// endpoint; no binding means default resolution, the provider's built-in
/// endpoint. `None` when that endpoint is not configured in this process (a
/// session created by another surface, or an endpoint since removed).
#[must_use]
pub fn endpoint_of(
    provider: Provider,
    auth_binding: Option<&AuthBindingRef>,
    endpoints: &EndpointRegistry,
) -> Option<String> {
    let name = match auth_binding {
        Some(binding) if is_endpoint_realm(binding.realm.as_str()) => binding.binding.as_str(),
        Some(_) => return None,
        None => WireDialect::from_provider(provider)?.as_str(),
    };
    endpoints.get(name).map(|e| e.name.clone())
}
