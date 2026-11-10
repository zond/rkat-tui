//! `watch_paths` and `unwatch`: an agent asks to be told when files of
//! the workspace change (P6 item B).
//!
//! The tools only register and drop watches with a [`WatchHost`]; the
//! daemon owns the watching itself (`crate::daemon::watches`): its process
//! holds the file-system watch, never the agent's confined shell, and it
//! delivers each change to the agent as a message into its session. Both
//! tools are read-only: they change nothing in the workspace, and what a
//! later event makes the agent do passes the gate like anything else.

use std::sync::Arc;

use async_trait::async_trait;
use meerkat_core::ops::ToolDispatchOutcome;
use meerkat_core::{
    AgentToolDispatcher, ToolCallView, ToolDef, ToolError, ToolMutationClass, ToolResult,
};
use serde::Deserialize;
use serde_json::json;

/// Tool name: register a watch.
pub const WATCH_PATHS: &str = "watch_paths";
/// Tool name: drop watches.
pub const UNWATCH: &str = "unwatch";
/// Provenance source id of the watch tools (judged with the file tools'
/// family, see `crate::approval::catalog`).
pub const WATCH_TOOLS_SOURCE: &str = "rkat-tui.watch";

/// Where an agent's watches are kept.
pub trait WatchHost: Send + Sync {
    /// Watch the workspace files `patterns` (globs relative to the
    /// workspace) for `agent`; `once`: drop the watch after its first
    /// event. Returns the watch's id, or why it cannot be watched.
    ///
    /// # Errors
    ///
    /// A message for the model (an invalid pattern, the watch could not be
    /// set up).
    fn watch(&self, agent: &str, patterns: &[String], once: bool) -> Result<u64, String>;

    /// Drop `agent`'s watch `id`, or all of them (`None`). Returns how many
    /// were dropped.
    fn unwatch(&self, agent: &str, id: Option<u64>) -> usize;
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchArgs {
    patterns: Vec<String>,
    #[serde(default)]
    once: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnwatchArgs {
    #[serde(default)]
    id: Option<u64>,
}

/// One agent's watch tools.
pub struct WatchToolDispatcher {
    agent: String,
    host: Arc<dyn WatchHost>,
    tools: Arc<[Arc<ToolDef>]>,
}

impl std::fmt::Debug for WatchToolDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchToolDispatcher")
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

impl WatchToolDispatcher {
    /// `agent`'s watch tools over `host`.
    #[must_use]
    pub fn new(agent: impl Into<String>, host: Arc<dyn WatchHost>) -> Self {
        let defs = [
            ToolDef::new(
                WATCH_PATHS,
                "Be told when workspace files change. `patterns` are globs relative to the workspace (`src/**`, `*.toml`, `target/report.json`). While the watch lives, each change to a matching file reaches you as a message `files changed: <paths>` (changes within 250 ms are batched, at most 50 paths named). `once: true` drops the watch after its first message and keeps it until then, past this turn (a later message then starts a turn of its own); otherwise it lives until this turn ends, `unwatch`, or you retire. Changes your own write_file/edit_file make are not reported. Returns the watch id.",
                json!({
                    "type": "object",
                    "properties": {
                        "patterns": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                        "once": {"type": "boolean"}
                    },
                    "required": ["patterns"],
                    "additionalProperties": false
                }),
            ),
            ToolDef::new(
                UNWATCH,
                "Drop a watch `watch_paths` made (its `id`), or every watch of yours (no id). Returns how many were dropped.",
                json!({
                    "type": "object",
                    "properties": {"id": {"type": "integer", "minimum": 0}},
                    "additionalProperties": false
                }),
            ),
        ];
        let tools: Arc<[Arc<ToolDef>]> = defs
            .into_iter()
            .map(|def| Arc::new(def.with_provenance(super::provenance(WATCH_TOOLS_SOURCE))))
            .collect();
        Self {
            agent: agent.into(),
            host,
            tools,
        }
    }
}

#[async_trait]
impl AgentToolDispatcher for WatchToolDispatcher {
    fn tools(&self) -> Arc<[Arc<ToolDef>]> {
        Arc::clone(&self.tools)
    }

    fn tool_mutation_class(&self, tool_name: &str) -> ToolMutationClass {
        match tool_name {
            WATCH_PATHS | UNWATCH => ToolMutationClass::ReadOnly,
            _ => ToolMutationClass::Unknown,
        }
    }

    async fn dispatch(&self, call: ToolCallView<'_>) -> Result<ToolDispatchOutcome, ToolError> {
        let id = call.id.to_string();
        let invalid = |err: serde_json::Error| ToolError::InvalidArguments {
            name: call.name.to_string(),
            reason: err.to_string(),
        };
        let result = match call.name {
            WATCH_PATHS => {
                let args: WatchArgs = call.parse_args().map_err(invalid)?;
                match self.host.watch(&self.agent, &args.patterns, args.once) {
                    Ok(watch) => ToolResult::new(
                        id,
                        format!(
                            "watching {} as watch {watch}{}",
                            args.patterns.join(", "),
                            if args.once {
                                " (once; it outlives this turn)"
                            } else {
                                " (until this turn ends)"
                            }
                        ),
                        false,
                    ),
                    Err(why) => ToolResult::new(id, why, true),
                }
            }
            UNWATCH => {
                let args: UnwatchArgs = call.parse_args().map_err(invalid)?;
                let dropped = self.host.unwatch(&self.agent, args.id);
                ToolResult::new(id, format!("dropped {dropped} watch(es)"), false)
            }
            other => {
                return Err(ToolError::NotFound {
                    name: other.to_string(),
                });
            }
        };
        Ok(ToolDispatchOutcome::from(result))
    }
}
