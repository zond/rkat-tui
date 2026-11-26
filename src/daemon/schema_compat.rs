//! Tool parameter schemas every provider dialect accepts.
//!
//! The engine derives most tool schemas with `schemars`: an optional enum
//! field comes out as `{"anyOf": [{"$ref": "#/$defs/X"}, {"type": "null"}]}`
//! with no `type` of its own. Anthropic and OpenAI accept that; Vertex AI's
//! function declarations (a Gemini model behind an OpenAI-compatible proxy
//! such as LiteLLM) refuse a property without a `type` with a 400, so such
//! an agent cannot run at all. The engine's own Gemini adapter lowers its
//! schemas; its OpenAI dialect folds only root-level combinators
//! (`meerkat-openai` `tool_schema.rs`).
//!
//! [`decorator`] wraps every agent-facing LLM client the daemon's runtime
//! builds (`SessionRuntime::set_agent_llm_client_decorator`), so every
//! model request of the agents and their helpers carries its tools through
//! [`normalize`]: local `$ref`s are inlined (and `$defs` dropped once none
//! is left), a `null` member of `anyOf`/`oneOf` is dropped and a single
//! member left is merged into its parent, a type list `[T, "null"]`
//! becomes `T`, and a schema without `type` gets the one its
//! `properties`, `items` or literals imply. What the model sees changes
//! only in form: no argument a tool accepts is refused by it (the tools
//! validate their own arguments). The model is only not offered an
//! explicit `null` for an optional field, which it can leave out.
//!
//! One case stays as it was: a recursive type keeps its `$ref` past
//! [`MAX_REF_DEPTH`] (and so its `$defs`), and a strict backend may still
//! refuse that one tool.
//!
//! The same decorator sends each request's messages without blank text
//! (`super::blank_text`: an assistant's empty or whitespace-only text
//! part, a tool result with no text), which a proxy would otherwise
//! replace with a placeholder sentence the model takes for its own words,
//! and without the engine's instructions about the comms tools rkat-tui
//! does not offer (`super::comms_text`: in the system prompt and in
//! `send_message`'s description).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use meerkat_core::{
    AgentError, AgentLlmClient, AgentLlmClientDecorator, AgentLlmRequestAttempt, LlmStreamResult,
    Message, ProviderParamsOverride, RequestAttemptAuthority, ToolDef,
};
use parking_lot::Mutex;
use serde_json::{Map, Value};

use super::{blank_text, call_ids, comms_text};
use crate::endpoints::EndpointRegistry;

/// How deep `$ref`s are inlined into each other (a recursive type stops
/// there and keeps its `$ref`).
const MAX_REF_DEPTH: usize = 8;

/// `schema` in the form every dialect accepts (see the module docs).
#[must_use]
pub fn normalize(schema: &Value) -> Value {
    let mut out = inline_refs(schema, schema, 0);
    if !contains_ref(&out)
        && let Value::Object(root) = &mut out
    {
        root.remove("$defs");
        root.remove("definitions");
    }
    lower(&mut out);
    out
}

/// Every `$ref` into the schema's own `$defs` / `definitions` replaced by
/// what it names (its siblings kept, the target's keys filling in).
fn inline_refs(root: &Value, value: &Value, depth: usize) -> Value {
    match value {
        Value::Object(obj) => {
            if let Some(Value::String(reference)) = obj.get("$ref")
                && depth < MAX_REF_DEPTH
                && let Some(target) = resolve(root, reference)
            {
                let mut merged = match inline_refs(root, target, depth + 1) {
                    Value::Object(target) => target,
                    other => return other,
                };
                for (key, value) in obj {
                    if key != "$ref" {
                        merged.insert(key.clone(), inline_refs(root, value, depth));
                    }
                }
                return Value::Object(merged);
            }
            Value::Object(
                obj.iter()
                    .map(|(key, value)| {
                        let inlined = if key == "$defs" || key == "definitions" {
                            value.clone()
                        } else {
                            inline_refs(root, value, depth)
                        };
                        (key.clone(), inlined)
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| inline_refs(root, item, depth))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The schema a local reference (`#/$defs/X`, `#/definitions/X`) names.
fn resolve<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    root.pointer(pointer)
}

fn contains_ref(value: &Value) -> bool {
    match value {
        Value::Object(obj) => obj.iter().any(|(key, value)| {
            key == "$ref" || (key != "$defs" && key != "definitions" && contains_ref(value))
        }),
        Value::Array(items) => items.iter().any(contains_ref),
        _ => false,
    }
}

/// Whether `schema` admits only `null`.
fn is_null_only(schema: &Value) -> bool {
    let Value::Object(obj) = schema else {
        return false;
    };
    obj.get("type").and_then(Value::as_str) == Some("null")
        || obj.get("const") == Some(&Value::Null)
        || obj
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|members| members.iter().all(Value::is_null))
}

/// One schema node and its children, in place.
fn lower(value: &mut Value) {
    let Value::Object(obj) = value else {
        return;
    };
    boolean_schemas_as_objects(obj);
    for key in ["anyOf", "oneOf"] {
        fold_nullable(obj, key);
    }
    one_of_as_any_of(obj);
    single_type(obj);
    if let Some(Value::Object(properties)) = obj.get_mut("properties") {
        for schema in properties.values_mut() {
            lower(schema);
        }
    }
    for key in ["items", "additionalProperties", "not"] {
        if let Some(child @ Value::Object(_)) = obj.get_mut(key) {
            lower(child);
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(members)) = obj.get_mut(key) {
            for member in members {
                lower(member);
            }
        }
    }
    infer_type(obj);
}

/// The boolean schemas among a node's children in their object form:
/// `true` ("any value") as `{}`, `false` ("no value") as `{"not": {}}`.
/// `schemars` writes `true` for a free-form JSON field and puts it in the
/// `anyOf` of an enum with a free-form variant; both are valid JSON Schema,
/// but a converter that expects every sub-schema to be an object (LiteLLM's
/// for Gemini: "argument of type 'bool' is not iterable") fails the whole
/// request on them. A union with a member that admits anything admits
/// anything, so it is dropped; an `allOf` loses such members.
/// `additionalProperties` keeps its boolean: that keyword's own form.
fn boolean_schemas_as_objects(obj: &mut Map<String, Value>) {
    fn as_object(schema: &mut Value) {
        match schema {
            Value::Bool(true) => *schema = Value::Object(Map::new()),
            Value::Bool(false) => {
                let mut never = Map::new();
                never.insert("not".to_string(), Value::Object(Map::new()));
                *schema = Value::Object(never);
            }
            _ => {}
        }
    }
    let anything = |schema: &Value| matches!(schema, Value::Object(map) if map.is_empty());
    if let Some(Value::Object(properties)) = obj.get_mut("properties") {
        properties.values_mut().for_each(as_object);
    }
    for key in ["items", "not"] {
        if let Some(child) = obj.get_mut(key) {
            as_object(child);
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(members)) = obj.get_mut(key) {
            members.iter_mut().for_each(as_object);
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(members)) = obj.get(key)
            && members.iter().any(anything)
        {
            obj.remove(key);
        }
    }
    if let Some(Value::Array(members)) = obj.get_mut("allOf") {
        members.retain(|member| !anything(member));
        if members.is_empty() {
            obj.remove("allOf");
        }
    }
}

/// `anyOf`/`oneOf` without its `null` members; one member left is merged
/// into the node (the node's own keys, such as its description, win).
fn fold_nullable(obj: &mut Map<String, Value>, key: &str) {
    let Some(Value::Array(members)) = obj.get_mut(key) else {
        return;
    };
    let before = members.len();
    members.retain(|member| !is_null_only(member));
    if members.len() == before || members.len() != 1 {
        if members.is_empty() {
            obj.remove(key);
        }
        return;
    }
    let Some(Value::Array(mut members)) = obj.remove(key) else {
        return;
    };
    let Some(Value::Object(member)) = members.pop() else {
        return;
    };
    for (name, value) in member {
        obj.entry(name).or_insert(value);
    }
}

/// A `oneOf` as `anyOf`: Vertex AI's schema subset (like OpenAI's strict
/// one) declares only `anyOf`, and the tool still checks that one variant
/// fits when it parses its arguments.
fn one_of_as_any_of(obj: &mut Map<String, Value>) {
    if obj.contains_key("anyOf") {
        return;
    }
    if let Some(members) = obj.remove("oneOf") {
        obj.insert("anyOf".to_string(), members);
    }
}

/// `type: [T, "null"]` as `type: T`.
fn single_type(obj: &mut Map<String, Value>) {
    let Some(Value::Array(types)) = obj.get("type") else {
        return;
    };
    let named: Vec<Value> = types
        .iter()
        .filter(|t| t.as_str() != Some("null"))
        .cloned()
        .collect();
    if named.len() == 1 {
        obj.insert(
            "type".to_string(),
            named.into_iter().next().unwrap_or(Value::Null),
        );
    }
}

/// The `type` a node without one implies: an object by its properties, an
/// array by its items, a literal type by its `enum`/`const`, or the type
/// all its `anyOf`/`oneOf` members share.
fn infer_type(obj: &mut Map<String, Value>) {
    if obj.contains_key("type") {
        return;
    }
    let inferred = if obj.contains_key("properties") || obj.contains_key("required") {
        Some("object")
    } else if obj.contains_key("items") || obj.contains_key("prefixItems") {
        Some("array")
    } else if let Some(literal) = obj.get("const") {
        literal_type(literal)
    } else if let Some(Value::Array(members)) = obj.get("enum") {
        common(members.iter().map(literal_type))
    } else {
        ["anyOf", "oneOf"]
            .iter()
            .find_map(|key| match obj.get(*key) {
                Some(Value::Array(members)) if !members.is_empty() => {
                    common(members.iter().map(|member| {
                        member
                            .get("type")
                            .and_then(Value::as_str)
                            .and_then(known_type)
                    }))
                }
                _ => None,
            })
    };
    if let Some(kind) = inferred {
        obj.insert("type".to_string(), Value::String(kind.to_string()));
    }
}

fn known_type(name: &str) -> Option<&'static str> {
    ["object", "array", "string", "integer", "number", "boolean"]
        .into_iter()
        .find(|known| *known == name)
}

fn literal_type(value: &Value) -> Option<&'static str> {
    match value {
        Value::String(_) => Some("string"),
        Value::Number(n) if n.is_i64() || n.is_u64() => Some("integer"),
        Value::Number(_) => Some("number"),
        Value::Bool(_) => Some("boolean"),
        Value::Array(_) => Some("array"),
        Value::Object(_) => Some("object"),
        Value::Null => None,
    }
}

/// The one type every item names (`None` when they differ or one names none).
fn common(mut types: impl Iterator<Item = Option<&'static str>>) -> Option<&'static str> {
    let first = types.next()??;
    types.all(|t| t == Some(first)).then_some(first)
}

/// Whether `obj` admits any JSON value (only annotations, no constraint):
/// left without a `type`, since none fits every value it admits. Gemini
/// accepts such an empty schema (`meerkat-gemini` `client.rs`,
/// `lower_boolean_child_schemas`: "verified accepted by the API").
#[must_use]
#[cfg(any(test, feature = "test-support"))]
pub fn unconstrained(obj: &Map<String, Value>) -> bool {
    obj.keys().all(|key| {
        matches!(
            key.as_str(),
            "description" | "default" | "title" | "examples"
        )
    })
}

/// A request's `messages` as sent to `provider`: no blank text
/// ([`blank_text::for_the_wire`]), no instructions about comms tools
/// the members do not have ([`comms_text::for_the_wire`]) and no tool-call
/// id the provider refuses ([`call_ids::for_the_wire`]); `None` when
/// nothing changes.
fn as_sent(provider: meerkat_core::Provider, messages: &[Message]) -> Option<Vec<Message>> {
    let cleaned = blank_text::for_the_wire(messages);
    let current = cleaned.as_deref().unwrap_or(messages);
    let comms = comms_text::for_the_wire(current).or(cleaned);
    let current = comms.as_deref().unwrap_or(messages);
    call_ids::for_the_wire(provider, current).or(comms)
}

/// [`as_sent`] of a request's `messages`; the same `Arc` when nothing
/// changes.
fn wire_messages(
    provider: meerkat_core::Provider,
    messages: Arc<Vec<Message>>,
) -> Arc<Vec<Message>> {
    match as_sent(provider, &messages) {
        Some(sent) => Arc::new(sent),
        None => messages,
    }
}

/// The decorator the daemon's runtime applies to every agent-facing LLM
/// client: [`SchemaCompat`] around it, refusing every request of a model
/// `endpoints`' lists (`endpoints.<name>.models`) do not allow.
#[must_use]
pub fn decorator(endpoints: Arc<EndpointRegistry>) -> AgentLlmClientDecorator {
    Arc::new(move |inner| {
        Arc::new(SchemaCompat::new(inner).with_endpoints(Arc::clone(&endpoints)))
            as Arc<dyn AgentLlmClient>
    })
}

/// Whether `client`'s model may run, by `endpoints`' lists: the route the
/// client reports (its model, provider, self-hosted server and auth
/// binding: the factory's selection) is checked; a client that reports
/// none (or whose current model is not the selection's) is checked as one
/// that could be on any endpoint of its provider
/// ([`EndpointRegistry::check_unrouted`]). The backstop below every other
/// check: an agent's or helper's request of a model its endpoint's list
/// does not allow never leaves the daemon.
///
/// # Errors
///
/// [`AgentError::ConfigError`] with the refusal, which the run's failure
/// shows.
pub fn check_client(
    endpoints: &EndpointRegistry,
    client: &dyn AgentLlmClient,
) -> Result<(), AgentError> {
    let refused = |err: crate::endpoints::ModelNotAllowed| {
        AgentError::ConfigError(format!(
            "{err}; this request was not sent. Pick an allowed model with /model"
        ))
    };
    let model = client.model();
    match client.controller_model_selection() {
        Some(selection) => {
            endpoints
                .check_route(
                    selection.model(),
                    selection.provider(),
                    selection.self_hosted_server_id(),
                    selection.auth_binding(),
                )
                .map_err(refused)?;
            if selection.model() != model {
                endpoints
                    .check_unrouted(model, client.provider())
                    .map_err(refused)?;
            }
            Ok(())
        }
        None => endpoints
            .check_unrouted(model, client.provider())
            .map_err(refused),
    }
}

/// An agent-facing LLM client whose requests carry their tools with
/// [`normalize`]d schemas and their messages without blank text
/// ([`blank_text::for_the_wire`]) and without the engine's instructions
/// about comms tools the members do not have ([`comms_text`]). Every method of the trait is forwarded to
/// `inner` (a method left to its default would answer for `inner`, which
/// the engine documents as a decorator's duty); `tests/it/layout.rs` fails
/// when the trait declares a method this impl does not.
pub struct SchemaCompat {
    inner: Arc<dyn AgentLlmClient>,
    /// The normalized definition per tool name, with the definition it was
    /// made from (a tool whose definition changes is normalized again);
    /// shared with the client's forks.
    cache: Arc<Mutex<Normalized>>,
    /// The endpoints whose `endpoints.<name>.models` every request is
    /// checked against ([`check_client`]); none: no check.
    endpoints: Option<Arc<EndpointRegistry>>,
}

/// [`SchemaCompat`]'s cache.
type Normalized = HashMap<String, (Arc<ToolDef>, Arc<ToolDef>)>;

impl SchemaCompat {
    /// `inner` with normalized tool schemas.
    #[must_use]
    pub fn new(inner: Arc<dyn AgentLlmClient>) -> Self {
        Self {
            inner,
            cache: Arc::default(),
            endpoints: None,
        }
    }

    /// Every request checked against `endpoints`' lists first.
    #[must_use]
    pub fn with_endpoints(mut self, endpoints: Arc<EndpointRegistry>) -> Self {
        self.endpoints = Some(endpoints);
        self
    }

    /// The client's model may run ([`check_client`]).
    fn allowed(&self) -> Result<(), AgentError> {
        match &self.endpoints {
            Some(endpoints) => check_client(endpoints, self.inner.as_ref()),
            None => Ok(()),
        }
    }

    /// `tools` with normalized schemas.
    fn tools(&self, tools: &[Arc<ToolDef>]) -> Arc<[Arc<ToolDef>]> {
        let mut cache = self.cache.lock();
        tools
            .iter()
            .map(|def| {
                if let Some((from, made)) = cache.get(def.name.as_str())
                    && (Arc::ptr_eq(from, def)
                        || (from.input_schema == def.input_schema
                            && from.description == def.description))
                {
                    return Arc::clone(made);
                }
                let mut made = ToolDef::clone(def);
                made.input_schema = normalize(&def.input_schema);
                if let Some(description) = comms_text::description(def) {
                    made.description = description;
                }
                let made = Arc::new(made);
                cache.insert(def.name.to_string(), (Arc::clone(def), Arc::clone(&made)));
                made
            })
            .collect()
    }
}

#[async_trait]
impl AgentLlmClient for SchemaCompat {
    fn controller_model_selection(&self) -> Option<meerkat_core::ControllerModelSelection> {
        self.inner.controller_model_selection()
    }

    fn controller_model_facts(
        &self,
    ) -> Result<meerkat_core::ControllerModelFacts, meerkat_core::ControllerFactsUnavailable> {
        self.inner.controller_model_facts()
    }

    async fn prepare_controller_credential(&self) -> Result<(), meerkat_core::auth::AuthError> {
        self.inner.prepare_controller_credential().await
    }

    fn reasoning_route(&self) -> Option<meerkat_core::ReasoningRouteObservation> {
        self.inner.reasoning_route()
    }

    fn default_provider_tag(&self) -> Option<meerkat_core::lifecycle::run_primitive::ProviderTag> {
        self.inner.default_provider_tag()
    }

    fn pin_controller(self: Arc<Self>) -> Option<meerkat_core::ControllerModelClient> {
        Arc::clone(&self.inner).pin_controller()
    }

    fn prepare_request_attempt_authorized(
        self: Arc<Self>,
        messages: Arc<Vec<Message>>,
        tools: Arc<[Arc<ToolDef>]>,
        max_tokens: u32,
        temperature: Option<f32>,
        provider_params: Option<ProviderParamsOverride>,
        authorization: Option<meerkat_core::LlmRequestAuthorization>,
    ) -> Result<Arc<dyn AgentLlmRequestAttempt>, AgentError> {
        self.allowed()?;
        let tools = self.tools(&tools);
        let messages = wire_messages(self.inner.provider(), messages);
        Arc::clone(&self.inner).prepare_request_attempt_authorized(
            messages,
            tools,
            max_tokens,
            temperature,
            provider_params,
            authorization,
        )
    }

    fn prepare_request_attempt(
        self: Arc<Self>,
        messages: Arc<Vec<Message>>,
        tools: Arc<[Arc<ToolDef>]>,
        max_tokens: u32,
        temperature: Option<f32>,
        provider_params: Option<ProviderParamsOverride>,
    ) -> Result<Arc<dyn AgentLlmRequestAttempt>, AgentError> {
        self.allowed()?;
        let tools = self.tools(&tools);
        let messages = wire_messages(self.inner.provider(), messages);
        Arc::clone(&self.inner).prepare_request_attempt(
            messages,
            tools,
            max_tokens,
            temperature,
            provider_params,
        )
    }

    fn request_attempt_authority(&self) -> RequestAttemptAuthority {
        self.inner.request_attempt_authority()
    }

    async fn stream_response(
        &self,
        messages: &[Message],
        tools: &[Arc<ToolDef>],
        max_tokens: u32,
        temperature: Option<f32>,
        provider_params: Option<&ProviderParamsOverride>,
    ) -> Result<LlmStreamResult, AgentError> {
        self.allowed()?;
        let tools = self.tools(tools);
        let sent = as_sent(self.inner.provider(), messages);
        let messages = sent.as_deref().unwrap_or(messages);
        self.inner
            .stream_response(messages, &tools, max_tokens, temperature, provider_params)
            .await
    }

    async fn stream_response_authorized(
        &self,
        messages: &[Message],
        tools: &[Arc<ToolDef>],
        max_tokens: u32,
        temperature: Option<f32>,
        provider_params: Option<&ProviderParamsOverride>,
        authorization: Option<meerkat_core::LlmRequestAuthorization>,
    ) -> Result<LlmStreamResult, AgentError> {
        self.allowed()?;
        let tools = self.tools(tools);
        let sent = as_sent(self.inner.provider(), messages);
        let messages = sent.as_deref().unwrap_or(messages);
        self.inner
            .stream_response_authorized(
                messages,
                &tools,
                max_tokens,
                temperature,
                provider_params,
                authorization,
            )
            .await
    }

    fn request_pressure(
        &self,
        messages: &[Message],
        tools: &[Arc<ToolDef>],
        max_tokens: u32,
        temperature: Option<f32>,
        provider_params: Option<&ProviderParamsOverride>,
    ) -> Result<Option<meerkat_core::ProviderRequestPressure>, AgentError> {
        let tools = self.tools(tools);
        let sent = as_sent(self.inner.provider(), messages);
        let messages = sent.as_deref().unwrap_or(messages);
        self.inner
            .request_pressure(messages, &tools, max_tokens, temperature, provider_params)
    }

    fn target_cache_lowering_capabilities(
        &self,
        issuer: &meerkat_core::TargetCacheLoweringIssuer,
        messages: &[Message],
        tools: &[Arc<ToolDef>],
        max_tokens: u32,
        temperature: Option<f32>,
        provider_params: Option<&ProviderParamsOverride>,
    ) -> Result<Vec<meerkat_core::TargetCacheLoweringCapability>, AgentError> {
        let tools = self.tools(tools);
        let sent = as_sent(self.inner.provider(), messages);
        let messages = sent.as_deref().unwrap_or(messages);
        self.inner.target_cache_lowering_capabilities(
            issuer,
            messages,
            &tools,
            max_tokens,
            temperature,
            provider_params,
        )
    }

    fn provider(&self) -> meerkat_core::Provider {
        self.inner.provider()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn fork_noncommitting_live_bridge(&self) -> Result<Arc<dyn AgentLlmClient>, AgentError> {
        // The fork's requests carry tools too.
        let fork = self.inner.fork_noncommitting_live_bridge()?;
        Ok(Arc::new(Self {
            inner: fork,
            cache: Arc::clone(&self.cache),
            endpoints: self.endpoints.clone(),
        }))
    }

    fn prepare_model_fallback(
        &self,
        failure: &AgentError,
        request: &meerkat_core::model_fallback::ModelFallbackRequest<'_>,
    ) -> Result<
        meerkat_core::AgentLlmFallbackSwitch,
        Vec<meerkat_core::AgentLlmFallbackSkippedTarget>,
    > {
        self.inner.prepare_model_fallback(failure, request)
    }

    fn commit_model_fallback(
        &self,
        previous_identity: &meerkat_core::SessionLlmIdentity,
        target_identity: &meerkat_core::SessionLlmIdentity,
    ) -> Result<(), AgentError> {
        // An engine fallback to a model the lists do not allow is refused
        // before it is switched to (and every request is checked anyway).
        if let Some(endpoints) = &self.endpoints {
            endpoints.check_identity(target_identity).map_err(|err| {
                AgentError::ConfigError(format!("the fallback model is refused: {err}"))
            })?;
        }
        self.inner
            .commit_model_fallback(previous_identity, target_identity)
    }

    fn active_model_fallback_identity(&self) -> Option<meerkat_core::SessionLlmIdentity> {
        self.inner.active_model_fallback_identity()
    }

    fn compile_model_fallback_schema(
        &self,
        target_identity: &meerkat_core::SessionLlmIdentity,
        output_schema: &meerkat_core::OutputSchema,
    ) -> Result<meerkat_core::CompiledSchema, AgentError> {
        self.inner
            .compile_model_fallback_schema(target_identity, output_schema)
    }

    fn begin_stream_output_observation(&self) {
        self.inner.begin_stream_output_observation();
    }

    fn stream_output_observed(&self) -> bool {
        self.inner.stream_output_observed()
    }

    fn stream_activity_count(&self) -> Option<u64> {
        self.inner.stream_activity_count()
    }

    fn compile_schema(
        &self,
        output_schema: &meerkat_core::OutputSchema,
    ) -> Result<meerkat_core::CompiledSchema, meerkat_core::SchemaError> {
        self.inner.compile_schema(output_schema)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every schema node under `value` that lacks a `type` (paths), the
    /// nodes Vertex AI's function declarations refuse; `$defs` and `$ref`
    /// count as failures too.
    pub(crate) fn untyped(value: &Value, path: &str, out: &mut Vec<String>) {
        let Value::Object(obj) = value else {
            return;
        };
        if obj.contains_key("$ref") || obj.contains_key("$defs") || obj.contains_key("definitions")
        {
            out.push(format!("{path}: $ref/$defs"));
        }
        if obj.contains_key("oneOf") {
            out.push(format!("{path}: oneOf"));
        }
        let union = obj.contains_key("anyOf");
        match obj.get("type") {
            Some(Value::String(_)) => {}
            _ if union || unconstrained(obj) => {}
            _ => out.push(path.to_string()),
        }
        if let Some(Value::Object(properties)) = obj.get("properties") {
            for (name, schema) in properties {
                untyped(schema, &format!("{path}.{name}"), out);
            }
        }
        if let Some(items) = obj.get("items") {
            untyped(items, &format!("{path}[]"), out);
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(Value::Array(members)) = obj.get(key) {
                for (n, member) in members.iter().enumerate() {
                    untyped(member, &format!("{path}|{n}"), out);
                }
            }
        }
    }

    /// The shape `schemars` gives `task_list`'s `status: Option<TaskStatus>`.
    fn task_list_like() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "status": {
                    "description": "Filter by status",
                    "anyOf": [{"$ref": "#/$defs/TaskStatus"}, {"type": "null"}]
                },
                "labels": {"type": ["array", "null"], "items": {"type": "string"}},
                "limit": {"type": ["integer", "null"], "format": "uint", "minimum": 0}
            },
            "required": [],
            "$defs": {
                "TaskStatus": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
            }
        })
    }

    #[test]
    fn an_optional_enum_gets_its_type_and_loses_the_ref() {
        let out = normalize(&task_list_like());
        assert_eq!(
            out["properties"]["status"],
            json!({
                "description": "Filter by status",
                "type": "string",
                "enum": ["pending", "in_progress", "completed"]
            })
        );
        assert_eq!(out["properties"]["labels"]["type"], "array");
        assert_eq!(out["properties"]["limit"]["type"], "integer");
        assert!(out.get("$defs").is_none(), "{out}");
        let mut missing = Vec::new();
        untyped(&out, "$", &mut missing);
        assert!(missing.is_empty(), "{missing:?}");
    }

    #[test]
    fn a_schema_in_the_accepted_form_is_unchanged() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "a file"},
                "lines": {"type": "array", "items": {"type": "integer"}},
                "mode": {"anyOf": [{"type": "string"}, {"type": "integer"}]}
            },
            "required": ["path"]
        });
        assert_eq!(normalize(&schema), schema);
    }

    #[test]
    fn nested_refs_unions_and_literals_are_typed() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {"$ref": "#/$defs/Spec"},
                "kind": {"const": "x"},
                "either": {"oneOf": [{"type": "string"}, {"type": "string", "format": "uri"}]}
            },
            "$defs": {
                "Spec": {
                    "properties": {
                        "inner": {"anyOf": [{"$ref": "#/$defs/Leaf"}, {"type": "null"}]}
                    }
                },
                "Leaf": {"enum": [1, 2, 3]}
            }
        });
        let out = normalize(&schema);
        assert_eq!(out["properties"]["spec"]["type"], "object");
        assert_eq!(
            out["properties"]["spec"]["properties"]["inner"]["type"],
            "integer"
        );
        assert_eq!(out["properties"]["kind"]["type"], "string");
        assert_eq!(out["properties"]["either"]["type"], "string");
        let mut missing = Vec::new();
        untyped(&out, "$", &mut missing);
        assert!(missing.is_empty(), "{missing:?}");
    }

    /// `schemars` writes a free-form JSON field as the boolean schema
    /// `true`, alone or in an enum's `anyOf`; a converter that expects
    /// objects fails the request on it. They become objects, and a union
    /// that admits anything is dropped; `additionalProperties` keeps its
    /// boolean. Normalizing again changes nothing (`/context` does).
    #[test]
    fn boolean_schemas_become_objects() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "free": true,
                "never": false,
                "either": {"description": "a name or anything", "anyOf": [{"type": "string"}, true]},
                "list": {"type": "array", "items": true},
                "both": {"allOf": [{"type": "integer"}, true]},
                "deep": {"type": "object", "properties": {"x": {"oneOf": [true, {"type": "null"}]}}}
            }
        });
        let out = normalize(&schema);
        assert!(!contains_boolean_schema(&out), "{out:#}");
        assert_eq!(out["additionalProperties"], json!(false));
        assert_eq!(out["properties"]["free"], json!({}));
        assert_eq!(out["properties"]["never"], json!({"not": {}}));
        assert_eq!(
            out["properties"]["either"],
            json!({"description": "a name or anything"})
        );
        assert_eq!(
            out["properties"]["list"],
            json!({"type": "array", "items": {}})
        );
        assert_eq!(
            out["properties"]["both"]["allOf"],
            json!([{"type": "integer"}])
        );
        assert_eq!(out["properties"]["deep"]["properties"]["x"], json!({}));
        assert_eq!(normalize(&out), out);
    }

    /// Whether a boolean stands where a sub-schema does.
    fn contains_boolean_schema(value: &Value) -> bool {
        let Value::Object(obj) = value else {
            return false;
        };
        let children = |key: &str| -> Vec<&Value> {
            match obj.get(key) {
                Some(Value::Object(map)) if key == "properties" => map.values().collect(),
                Some(Value::Array(members)) => members.iter().collect(),
                Some(other) => vec![other],
                None => Vec::new(),
            }
        };
        [
            "properties",
            "items",
            "not",
            "anyOf",
            "oneOf",
            "allOf",
            "prefixItems",
        ]
        .into_iter()
        .flat_map(children)
        .any(|child| child.is_boolean() || contains_boolean_schema(child))
    }

    #[test]
    fn normalizing_twice_is_normalizing_once() {
        for schema in [
            task_list_like(),
            json!({"oneOf": [{"type": "string"}, {"type": "null"}]}),
        ] {
            let once = normalize(&schema);
            assert_eq!(normalize(&once), once);
        }
    }

    #[test]
    fn a_recursive_ref_stays_with_its_defs() {
        let schema = json!({
            "type": "object",
            "properties": {"node": {"$ref": "#/$defs/Node"}},
            "$defs": {"Node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/Node"}}}}
        });
        let out = normalize(&schema);
        assert!(
            out.get("$defs").is_some(),
            "the deepest $ref still needs them"
        );
        assert_eq!(out["properties"]["node"]["type"], "object");
    }

    /// A client that counts its requests and reports no route.
    struct Counting {
        model: String,
        provider: meerkat_core::Provider,
        sent: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl AgentLlmClient for Counting {
        async fn stream_response(
            &self,
            _messages: &[Message],
            _tools: &[Arc<ToolDef>],
            _max_tokens: u32,
            _temperature: Option<f32>,
            _provider_params: Option<&ProviderParamsOverride>,
        ) -> Result<LlmStreamResult, AgentError> {
            self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(LlmStreamResult::new(
                Vec::new(),
                meerkat_core::StopReason::EndTurn,
                meerkat_core::Usage::default(),
            ))
        }

        fn provider(&self) -> meerkat_core::Provider {
            self.provider
        }

        fn model(&self) -> &str {
            &self.model
        }
    }

    /// The backstop: a request of a model the endpoint's list does not
    /// allow is refused in the decorator, naming the setting, and never
    /// reaches the client below; an allowed one goes through, and so does
    /// one on an endpoint without a list. A fallback onto a forbidden
    /// model is refused before it is switched to.
    #[tokio::test]
    async fn the_decorator_refuses_a_model_its_endpoint_does_not_allow() {
        use crate::endpoints::{ApiStyle, Endpoint, SecretString, WireDialect};
        let gateway = Endpoint::configured(
            "gateway",
            WireDialect::OpenAi,
            Some("https://llm.example.net".into()),
            SecretString::new("plainly-fake"),
            "GATEWAY_KEY",
            0,
            ApiStyle::default(),
        )
        .with_models(Some(vec!["model-a".into()]));
        let endpoints = Arc::new(EndpointRegistry::from_endpoints(vec![
            Endpoint::builtin(WireDialect::Anthropic, None, SecretString::new("k")),
            gateway.clone(),
        ]));
        let send = |model: &str, provider| {
            let inner = Arc::new(Counting {
                model: model.to_string(),
                provider,
                sent: std::sync::atomic::AtomicUsize::new(0),
            });
            let client =
                (decorator(Arc::clone(&endpoints)))(Arc::clone(&inner) as Arc<dyn AgentLlmClient>);
            (inner, client)
        };
        let messages = [Message::User(meerkat_core::UserMessage::text("hi"))];
        let (inner, client) = send("gateway:model-z", meerkat_core::Provider::SelfHosted);
        let err = client
            .stream_response(&messages, &[], 100, None, None)
            .await
            .err()
            .expect("refused");
        assert!(
            err.to_string()
                .contains("endpoints.gateway.models allows only model-a"),
            "{err}"
        );
        let attempt = Arc::clone(&client).prepare_request_attempt(
            Arc::new(messages.to_vec()),
            Arc::from(Vec::new()),
            100,
            None,
            None,
        );
        assert!(attempt.is_err());
        assert_eq!(inner.sent.load(std::sync::atomic::Ordering::SeqCst), 0);
        for (model, provider) in [
            ("gateway:model-a", meerkat_core::Provider::SelfHosted),
            ("claude-anything", meerkat_core::Provider::Anthropic),
        ] {
            let (inner, client) = send(model, provider);
            client
                .stream_response(&messages, &[], 100, None, None)
                .await
                .unwrap();
            assert_eq!(
                inner.sent.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "{model}"
            );
        }
        let (_, client) = send("gateway:model-a", meerkat_core::Provider::SelfHosted);
        let identity = |model: &str| meerkat_core::SessionLlmIdentity {
            model: model.to_string(),
            provider: meerkat_core::Provider::SelfHosted,
            self_hosted_server_id: Some("gateway".into()),
            provider_params: None,
            auth_binding: gateway.binding(),
        };
        let err = client
            .commit_model_fallback(&identity("gateway:model-a"), &identity("gateway:model-z"))
            .unwrap_err();
        assert!(
            err.to_string().contains("the fallback model is refused"),
            "{err}"
        );
    }
}
