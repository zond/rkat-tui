//! `endpoint:model` references: parsing, resolving a bare model id to
//! the endpoint that serves it, and the engine's names for them.

use super::*;

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
pub(super) struct NameList<'a>(pub(super) &'a [String]);

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

pub(super) fn names(endpoints: &[Endpoint]) -> Vec<String> {
    endpoints.iter().map(|e| e.name.clone()).collect()
}

/// Split `name:model` when `name` is a configured endpoint.
pub(super) fn split_explicit<'a>(s: &'a str, endpoints: &[Endpoint]) -> Option<(&'a str, &'a str)> {
    let (name, model) = s.split_once(':')?;
    endpoints
        .iter()
        .any(|e| e.name == name)
        .then_some((name, model))
}

/// The endpoint serving a catalogued id: the built-in endpoint of its
/// provider's dialect when configured, else the only endpoint of that
/// dialect. Ids outside the catalog never resolve here.
pub(super) fn by_catalog<'a>(model: &str, endpoints: &'a [Endpoint]) -> Option<&'a Endpoint> {
    let dialect = WireDialect::of_catalog_model(model)?;
    if let Some(builtin) = endpoints
        .iter()
        .find(|e| e.is_builtin() && e.dialect == dialect)
    {
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
            .find(|e| e.is_builtin() && e.dialect == dialect)
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
