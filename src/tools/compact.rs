//! `compact_ready`: an agent tells the daemon it has prepared for a
//! compaction of its context (`[auto] compact_prepare`), or asks for one.
//!
//! The tool only hands the call to a [`CompactHost`]; the daemon decides
//! (`crate::daemon::prepare`): after its instruction, or with enough
//! context, it requests the compaction, which the engine applies before
//! the run's next model request. Read-only: it changes nothing in the
//! workspace.

use std::sync::Arc;

use async_trait::async_trait;
use meerkat_core::ops::ToolDispatchOutcome;
use meerkat_core::{
    AgentToolDispatcher, ToolCallView, ToolDef, ToolError, ToolMutationClass, ToolResult,
};
use serde_json::json;

/// Tool name.
pub const COMPACT_READY: &str = "compact_ready";
/// Provenance source id of the tool (judged with the file tools' family,
/// see `crate::approval::catalog`).
pub const COMPACT_TOOLS_SOURCE: &str = "rkat-tui.compact";

/// Where an agent's `compact_ready` goes.
#[async_trait]
pub trait CompactHost: Send + Sync {
    /// `agent` is ready for a compaction: what the model is told, and
    /// whether that is an error.
    async fn compact_ready(&self, agent: &str) -> (String, bool);
}

/// One agent's `compact_ready`.
pub struct CompactToolDispatcher {
    agent: String,
    host: Arc<dyn CompactHost>,
    tools: Arc<[Arc<ToolDef>]>,
}

impl std::fmt::Debug for CompactToolDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactToolDispatcher")
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

impl CompactToolDispatcher {
    /// `agent`'s `compact_ready` over `host`.
    #[must_use]
    pub fn new(agent: impl Into<String>, host: Arc<dyn CompactHost>) -> Self {
        let def = ToolDef::new(
            COMPACT_READY,
            "Compact your context now (after a prepare instruction, or when you need room).",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        )
        .with_provenance(super::provenance(COMPACT_TOOLS_SOURCE));
        Self {
            agent: agent.into(),
            host,
            tools: vec![Arc::new(def)].into(),
        }
    }
}

#[async_trait]
impl AgentToolDispatcher for CompactToolDispatcher {
    fn tools(&self) -> Arc<[Arc<ToolDef>]> {
        Arc::clone(&self.tools)
    }

    fn tool_catalog_capabilities(&self) -> meerkat_core::ToolCatalogCapabilities {
        crate::tools::fixed_catalog()
    }

    fn tool_mutation_class(&self, tool_name: &str) -> ToolMutationClass {
        match tool_name {
            COMPACT_READY => ToolMutationClass::ReadOnly,
            _ => ToolMutationClass::Unknown,
        }
    }

    async fn dispatch(&self, call: ToolCallView<'_>) -> Result<ToolDispatchOutcome, ToolError> {
        if call.name != COMPACT_READY {
            return Err(ToolError::NotFound {
                name: call.name.to_string(),
            });
        }
        let (text, is_error) = self.host.compact_ready(&self.agent).await;
        Ok(ToolDispatchOutcome::from(ToolResult::new(
            call.id.to_string(),
            text,
            is_error,
        )))
    }
}

/// The host of the tool catalog read at start: it compacts nothing.
pub struct NoCompactions;

#[async_trait]
impl CompactHost for NoCompactions {
    async fn compact_ready(&self, _agent: &str) -> (String, bool) {
        ("nothing was compacted here".to_string(), true)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    struct Answer;

    #[async_trait]
    impl CompactHost for Answer {
        async fn compact_ready(&self, agent: &str) -> (String, bool) {
            (format!("asked for {agent}"), false)
        }
    }

    /// One short, argument-free, read-only definition; a call goes to the
    /// host for the agent the dispatcher was built for.
    #[tokio::test]
    async fn the_tool_is_one_short_definition_that_asks_the_host() {
        let tools = CompactToolDispatcher::new("main-ab12", Arc::new(Answer));
        let defs = tools.tools();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name.as_str(), COMPACT_READY);
        assert!(defs[0].description.len() < 100, "{}", defs[0].description);
        assert_eq!(
            tools.tool_mutation_class(COMPACT_READY),
            ToolMutationClass::ReadOnly
        );
        let args = serde_json::value::RawValue::from_string("{}".into()).unwrap();
        let call = ToolCallView {
            id: "c1",
            name: COMPACT_READY,
            args: &args,
        };
        let outcome = tools.dispatch(call).await.unwrap();
        let text = format!("{outcome:?}");
        assert!(text.contains("asked for main-ab12"), "{text}");
    }
}
