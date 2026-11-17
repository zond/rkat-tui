//! `assign`: start a single-use helper on a self-contained task; and
//! `connect_helpers`: wire helpers the caller started to each other.
//!
//! The tools only hand the call, with the calling session, to an
//! [`AssignHost`]; the daemon seats the helper and answers at once
//! (`crate::daemon::assign`), or wires the helpers
//! (`crate::daemon::connect`). The helper runs beside its caller, and the
//! caller is told when it finishes (`crate::mobs::endings`).

use std::sync::Arc;

use async_trait::async_trait;
use meerkat_core::ops::ToolDispatchOutcome;
use meerkat_core::{
    AgentToolDispatcher, SessionId, ToolCallView, ToolDef, ToolDispatchContext, ToolError,
    ToolMutationClass, ToolResult,
};
use serde::Deserialize;
use serde_json::json;

/// Tool name.
pub const ASSIGN: &str = "assign";
/// Tool name.
pub const CONNECT_HELPERS: &str = "connect_helpers";
/// Provenance source id of the tools (judged with the file tools' family,
/// see `crate::approval::catalog`).
pub const ASSIGN_TOOLS_SOURCE: &str = "rkat-tui.assign";

/// The arguments of one `connect_helpers` call.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectArgs {
    /// The caller's own helpers to wire to each other, by name.
    pub helpers: Vec<String>,
}

/// A minimal valid `connect_helpers` call.
pub const CONNECT_EXAMPLE: &str = r#"{"helpers": ["scout", "tester"]}"#;

/// The arguments of one `assign` call.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssignArgs {
    /// The helper's name (its member id and peer name).
    pub name: String,
    /// Its first message: everything it knows of the work.
    pub task: String,
    /// The model it runs on (`None`: the settings' or the caller's).
    #[serde(default)]
    pub model: Option<String>,
}

/// Where an `assign` call goes.
#[async_trait]
pub trait AssignHost: Send + Sync {
    /// The session `caller` asks for a helper on `args`: what the model is
    /// told, and whether that is an error.
    async fn assign(&self, caller: Option<&SessionId>, args: AssignArgs) -> (String, bool);
    /// The session `caller` asks for its helpers `args` to be wired to
    /// each other: what the model is told, and whether that is an error.
    async fn connect(&self, caller: Option<&SessionId>, args: ConnectArgs) -> (String, bool);
}

/// The `assign` tool over a host (one for every agent and helper: the
/// caller is the call's own session).
pub struct AssignToolDispatcher {
    host: Arc<dyn AssignHost>,
    tools: Arc<[Arc<ToolDef>]>,
}

impl std::fmt::Debug for AssignToolDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssignToolDispatcher")
            .finish_non_exhaustive()
    }
}

/// A minimal valid call, for the errors that say how one must look.
pub const EXAMPLE: &str =
    r#"{"name": "scout", "task": "Find where the config file is parsed and say how."}"#;

impl AssignToolDispatcher {
    /// `assign` over `host`.
    #[must_use]
    pub fn new(host: Arc<dyn AssignHost>) -> Self {
        let def = ToolDef::new(
            ASSIGN,
            "Start a helper on a self-contained task. It runs alongside you, from a fresh context \
             (the task is all it knows), and you are told when it finishes, with its answer; it is \
             then retired. Meanwhile you can message each other.",
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Short name: lowercase letters, digits and '-', at most 32."
                    },
                    "task": {
                        "type": "string",
                        "description": "Its first message: the whole task and the context it needs."
                    },
                    "model": {
                        "type": "string",
                        "description": "Model to run on (default: the helpers' setting, else yours)."
                    }
                },
                "required": ["name", "task"],
                "additionalProperties": false
            }),
        )
        .with_provenance(super::provenance(ASSIGN_TOOLS_SOURCE));
        let connect = ToolDef::new(
            CONNECT_HELPERS,
            "Wire two or more of your own helpers (any kind you started: assign, fork_off, \
             spawn_member, mob_spawn_member in a mob you created; in one mob or in different \
             ones) to each other, every pair both ways, so they can message each other directly; \
             each is told. A pair stays connected until one of them leaves. Use it only for \
             helpers that must coordinate: each message wakes its receiver.",
            json!({
                "type": "object",
                "properties": {
                    "helpers": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 2,
                        "description": "Your helpers' names, as the call that started each one gave them."
                    }
                },
                "required": ["helpers"],
                "additionalProperties": false
            }),
        )
        .with_provenance(super::provenance(ASSIGN_TOOLS_SOURCE));
        Self {
            host,
            tools: vec![Arc::new(def), Arc::new(connect)].into(),
        }
    }

    async fn run(
        &self,
        call: ToolCallView<'_>,
        caller: Option<&SessionId>,
    ) -> Result<ToolDispatchOutcome, ToolError> {
        if call.name == CONNECT_HELPERS {
            let args: ConnectArgs =
                call.parse_args()
                    .map_err(|err| ToolError::InvalidArguments {
                        name: CONNECT_HELPERS.to_string(),
                        reason: format!("{err}; a valid call looks like {CONNECT_EXAMPLE}"),
                    })?;
            let (text, is_error) = self.host.connect(caller, args).await;
            return Ok(ToolDispatchOutcome::from(ToolResult::new(
                call.id.to_string(),
                text,
                is_error,
            )));
        }
        if call.name != ASSIGN {
            return Err(ToolError::NotFound {
                name: call.name.to_string(),
            });
        }
        let args: AssignArgs = call
            .parse_args()
            .map_err(|err| ToolError::InvalidArguments {
                name: ASSIGN.to_string(),
                reason: format!("{err}; a valid call looks like {EXAMPLE}"),
            })?;
        let (text, is_error) = self.host.assign(caller, args).await;
        Ok(ToolDispatchOutcome::from(ToolResult::new(
            call.id.to_string(),
            text,
            is_error,
        )))
    }
}

#[async_trait]
impl AgentToolDispatcher for AssignToolDispatcher {
    fn tools(&self) -> Arc<[Arc<ToolDef>]> {
        Arc::clone(&self.tools)
    }

    fn tool_catalog_capabilities(&self) -> meerkat_core::ToolCatalogCapabilities {
        crate::tools::fixed_catalog()
    }

    fn tool_mutation_class(&self, tool_name: &str) -> ToolMutationClass {
        match tool_name {
            // Seating a helper changes nothing by itself: every call the
            // helper makes passes its caller's gate. Wiring the caller's
            // own helpers lets them message each other, nothing more.
            ASSIGN | CONNECT_HELPERS => ToolMutationClass::ReadOnly,
            _ => ToolMutationClass::Unknown,
        }
    }

    async fn dispatch(&self, call: ToolCallView<'_>) -> Result<ToolDispatchOutcome, ToolError> {
        self.run(call, None).await
    }

    async fn dispatch_with_context(
        &self,
        call: ToolCallView<'_>,
        context: &ToolDispatchContext,
    ) -> Result<ToolDispatchOutcome, ToolError> {
        self.run(call, context.origin_session_id()).await
    }
}

/// The host of the tool catalog read at start: it seats nobody.
pub struct NoAssignments;

#[async_trait]
impl AssignHost for NoAssignments {
    async fn assign(&self, _caller: Option<&SessionId>, _args: AssignArgs) -> (String, bool) {
        ("no helper can be started here".to_string(), true)
    }

    async fn connect(&self, _caller: Option<&SessionId>, _args: ConnectArgs) -> (String, bool) {
        ("no helpers can be connected here".to_string(), true)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait]
    impl AssignHost for Echo {
        async fn assign(&self, caller: Option<&SessionId>, args: AssignArgs) -> (String, bool) {
            (
                format!("{} for {}: {}", args.name, caller.is_some(), args.task),
                false,
            )
        }

        async fn connect(&self, _caller: Option<&SessionId>, args: ConnectArgs) -> (String, bool) {
            (format!("connect {}", args.helpers.join("+")), false)
        }
    }

    /// One short definition naming `name` and `task` as required; a call
    /// goes to the host with its arguments, a malformed one is refused with
    /// a valid example.
    #[tokio::test]
    async fn the_tool_is_one_short_definition_that_asks_the_host() {
        let tools = AssignToolDispatcher::new(Arc::new(Echo));
        let defs = tools.tools();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name.as_str(), ASSIGN);
        assert_eq!(defs[1].name.as_str(), CONNECT_HELPERS);
        assert!(defs[1].description.len() < 400, "{}", defs[1].description);
        assert!(defs[0].description.len() < 300, "{}", defs[0].description);
        let schema = defs[0].input_schema.to_string();
        assert!(schema.contains(r#""required":["name","task"]"#), "{schema}");
        let args = serde_json::value::RawValue::from_string(r#"{"name":"a","task":"look"}"#.into())
            .unwrap();
        let call = ToolCallView {
            id: "c1",
            name: ASSIGN,
            args: &args,
        };
        let outcome = format!("{:?}", tools.dispatch(call).await.unwrap());
        assert!(outcome.contains("a for false: look"), "{outcome}");
        let bad = serde_json::value::RawValue::from_string(r#"{"task":"look"}"#.into()).unwrap();
        let call = ToolCallView {
            id: "c2",
            name: ASSIGN,
            args: &bad,
        };
        let err = tools.dispatch(call).await.unwrap_err().to_string();
        assert!(
            err.contains("name") && err.contains("a valid call"),
            "{err}"
        );
    }
}
