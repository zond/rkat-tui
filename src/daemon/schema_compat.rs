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
//! validate their own arguments), the model is only no longer offered an
//! explicit `null` for an optional field, which it can leave out.
//!
//! One case stays as it was: a recursive type keeps its `$ref` past
//! [`MAX_REF_DEPTH`] (and so its `$defs`), and a strict backend may still
//! refuse that one tool.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use meerkat_core::{
    AgentError, AgentLlmClient, AgentLlmClientDecorator, AgentLlmRequestAttempt, LlmStreamResult,
    Message, ProviderParamsOverride, RequestAttemptAuthority, ToolDef,
};
use parking_lot::Mutex;
use serde_json::{Map, Value};

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

/// The decorator the daemon's runtime applies to every agent-facing LLM
/// client: [`SchemaCompat`] around it.
#[must_use]
pub fn decorator() -> AgentLlmClientDecorator {
    Arc::new(|inner| Arc::new(SchemaCompat::new(inner)) as Arc<dyn AgentLlmClient>)
}

/// An agent-facing LLM client whose requests carry their tools with
/// [`normalize`]d schemas. Every method of the trait is forwarded to
/// `inner` (a method left to its default would answer for `inner`, which
/// the engine documents as a decorator's duty); `tests/it/layout.rs` fails
/// when the trait declares a method this impl does not.
pub struct SchemaCompat {
    inner: Arc<dyn AgentLlmClient>,
    /// The normalized definition per tool name, with the definition it was
    /// made from (a tool whose definition changes is normalized again);
    /// shared with the client's forks.
    cache: Arc<Mutex<Normalized>>,
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
        let tools = self.tools(&tools);
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
        let tools = self.tools(&tools);
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
        let tools = self.tools(tools);
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
        let tools = self.tools(tools);
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

    /// `/context` measures definitions normalized again: that changes
    /// nothing.
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
}
