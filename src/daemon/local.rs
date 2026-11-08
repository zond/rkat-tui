//! The daemon's own methods, served before the engine's router sees a
//! line, and what the daemon learns from the engine methods it forwards.
//!
//! The wire is Meerkat's JSON-RPC; the only non-engine methods are these
//! `rkat/*` ones (and [`super::handshake`]'s `rkat/attach`). They exist
//! because the approval policy lives in the daemon and the engine's wire
//! has nothing for an application's permission mode or rule list:
//!
//! - [`GET_MODE`] `{}` → [`ModeStatus`] (`{"mode", "paused", "denials"}`);
//! - [`SET_MODE`] `{"mode": "default"|"accept-edits"|"plan"|"auto"}` →
//!   `{"status": ModeStatus, "notice": "…", "warning"?: "…"}`: switches the
//!   daemon's permission mode (one per daemon: it governs the orchestrator
//!   and its helpers, like the in-process mode governs a session and its
//!   helpers), records it in the realm rules file and tells the
//!   orchestrator when plan mode starts or ends;
//! - [`RULES`] a [`RuleCommand`] (`{"op": "add"|"remove"|"list", …}`) →
//!   [`crate::approval::remote::RuleOutcome`]: `/rules` run on the daemon's
//!   engine, which persists to the realm rules file as the in-process one
//!   does.
//!
//! A future engine extension (an application-defined method namespace, or
//! the governed realm's own review policy) could replace them.
//!
//! What the forwarded methods tell the daemon ([`Local::observe`]): a
//! prompt or steer for the orchestrator (`mob/member_send`,
//! `mob/turn_start`) is the human's words, recorded in the classifier's
//! transcript as the in-process session records them at admission (the
//! connection proved it holds the daemon's token); an interrupt
//! (`mob/stop_member_run`, `mob/force_cancel`) stops the session's review
//! and answers its open escalations with Stop, as the in-process runner
//! does; and a `mob/turn_start` that switches the model carries the
//! `[thinking]` provider params for the new model unless it names its own.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use meerkat_core::{AppendSystemContextRequest, Provider, SessionId};
use meerkat_mob::{AgentIdentity, MobId};
use meerkat_mob_mcp::MobMcpState;
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::approvals::Desk;
use super::{MOB_ID, ORCHESTRATOR};
use crate::approval::mode::PermissionMode;
use crate::approval::remote::{ModeStatus, RuleCommand, mode_notice, run_rule_command};
use crate::approval::{ApprovalEngine, InputKey};
use crate::endpoints::WireDialect;
use crate::thinking::{self, ThinkingTable};

/// Every local method starts with this.
pub const PREFIX: &str = "rkat/";
/// The permission mode and the denial counters.
pub const GET_MODE: &str = "rkat/get_mode";
/// Switch the permission mode.
pub const SET_MODE: &str = "rkat/set_mode";
/// A `/rules` command.
pub const RULES: &str = "rkat/rules";

/// JSON-RPC "method not found".
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC "invalid params".
pub const INVALID_PARAMS: i64 = -32602;

/// The idempotency source of the daemon's system-context notes.
const SYSTEM_CONTEXT_SOURCE: &str = "rkat-tui";

/// What the local methods and the observer act on.
pub struct Local {
    engine: Arc<ApprovalEngine>,
    state: Arc<MobMcpState>,
    session: SessionId,
    desk: Arc<Desk>,
    thinking: ThinkingTable,
    /// The orchestrator was told plan mode is on.
    plan_noted: Mutex<bool>,
    inputs: AtomicU64,
}

impl std::fmt::Debug for Local {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Local")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// A local method's refusal: a JSON-RPC error code and message.
pub type Refusal = (i64, String);

impl Local {
    /// The local surface for the orchestrator running as `session`.
    #[must_use]
    pub fn new(
        engine: Arc<ApprovalEngine>,
        state: Arc<MobMcpState>,
        session: SessionId,
        desk: Arc<Desk>,
        thinking: ThinkingTable,
    ) -> Self {
        Self {
            engine,
            state,
            session,
            desk,
            thinking,
            plan_noted: Mutex::new(false),
            inputs: AtomicU64::new(0),
        }
    }

    /// Answer the local method `method`.
    ///
    /// # Errors
    ///
    /// [`METHOD_NOT_FOUND`] for an unknown `rkat/*` method,
    /// [`INVALID_PARAMS`] for bad parameters.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, Refusal> {
        match method {
            GET_MODE => Ok(json!(ModeStatus::of(&self.engine))),
            SET_MODE => {
                let mode = params
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(PermissionMode::parse)
                    .ok_or_else(|| {
                        (
                            INVALID_PARAMS,
                            "`mode` must be one of default, accept-edits, plan, auto".to_string(),
                        )
                    })?;
                Ok(self.set_mode(mode).await)
            }
            RULES => {
                let command: RuleCommand = serde_json::from_value(params)
                    .map_err(|err| (INVALID_PARAMS, err.to_string()))?;
                Ok(json!(run_rule_command(&self.engine, &command)))
            }
            other => Err((METHOD_NOT_FOUND, format!("unknown method `{other}`"))),
        }
    }

    async fn set_mode(&self, mode: PermissionMode) -> Value {
        self.engine.set_mode(mode);
        let mut warnings = Vec::new();
        if let Err(err) = self.engine.persist_mode() {
            warnings.push(format!(
                "the mode applies to this run but was not saved: {err}"
            ));
        }
        if let Some(warning) = self.note_plan_mode(mode == PermissionMode::Plan).await {
            warnings.push(warning);
        }
        let mut answer = json!({
            "status": ModeStatus::of(&self.engine),
            "notice": mode_notice(mode),
        });
        if !warnings.is_empty() {
            answer["warning"] = json!(warnings.join("\n"));
        }
        answer
    }

    /// Tell the orchestrator plan mode started or ended, once per change.
    async fn note_plan_mode(&self, plan: bool) -> Option<String> {
        if *self.plan_noted.lock() == plan {
            return None;
        }
        let note = if plan {
            crate::runner::approval::PLAN_ON_NOTE
        } else {
            crate::runner::approval::PLAN_OFF_NOTE
        };
        let mut request = AppendSystemContextRequest::from_text(note.to_string());
        request.source = Some(SYSTEM_CONTEXT_SOURCE.to_string());
        request.idempotency_key = Some(crate::runner::approval::plan_note_key(plan));
        match self
            .state
            .mob_append_system_context(
                &MobId::from(MOB_ID),
                &AgentIdentity::from(ORCHESTRATOR),
                request,
            )
            .await
        {
            Ok(_) => {
                *self.plan_noted.lock() = plan;
                None
            }
            Err(err) => Some(format!(
                "the agent could not be told about plan mode now (the tool errors say it): {err}"
            )),
        }
    }

    /// Learn from an engine request on its way to the router (see the
    /// module docs); returns the request as it is forwarded.
    #[must_use]
    pub fn observe(&self, mut request: Value) -> Value {
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some(params) = request.get_mut("params") else {
            return request;
        };
        let ours = params.get("mob_id").and_then(Value::as_str) == Some(MOB_ID)
            && params.get("agent_identity").and_then(Value::as_str) == Some(ORCHESTRATOR);
        if !ours {
            return request;
        }
        match method.as_str() {
            "mob/member_send" | "mob/turn_start" => {
                let field = if method == "mob/member_send" {
                    "content"
                } else {
                    "prompt"
                };
                if let Some(text) = params.get(field).and_then(content_text)
                    && !text.trim().is_empty()
                {
                    let n = self.inputs.fetch_add(1, Ordering::Relaxed);
                    let key = InputKey::new(format!("rkat-attach:{}:{n}", std::process::id()));
                    self.engine.record_user_input(&self.session, key, &text);
                }
                if method == "mob/turn_start" {
                    self.with_thinking(params);
                }
            }
            "mob/stop_member_run" | "mob/force_cancel" => {
                self.engine.stop(&self.session);
                self.desk.stop(&self.session);
            }
            _ => {}
        }
        request
    }

    /// A model switch carries the `[thinking]` the new model is configured
    /// with (or clears the old model's), unless the caller names params.
    fn with_thinking(&self, params: &mut Value) {
        if params.get("provider_params").is_some() {
            return;
        }
        let (Some(model), Some(provider)) = (
            params.get("model").and_then(Value::as_str),
            params.get("provider").and_then(Value::as_str),
        ) else {
            return;
        };
        let Some(dialect) = WireDialect::parse(provider) else {
            return;
        };
        let provider_kind: Provider = dialect.into();
        let configured = self.thinking.resolve(Some(provider), provider_kind, model);
        params["provider_params"] = thinking_override(configured.as_ref());
    }
}

/// The `provider_params` override of a model switch: set the thinking the
/// new model is configured with, or clear the old model's.
#[must_use]
pub fn thinking_override(
    thinking: Option<&meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig>,
) -> Value {
    match thinking::params_for(thinking) {
        Some(params) => json!({
            "action": "set",
            "value": meerkat_contracts::wire::runtime::WireProviderParamsOverride::from(params),
        }),
        None => json!({"action": "clear"}),
    }
}

/// The text of a wire content input: a string, or the text blocks of a
/// block list.
fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn content_text_reads_strings_and_text_blocks() {
        assert_eq!(content_text(&json!("hi")).as_deref(), Some("hi"));
        assert_eq!(
            content_text(&json!([{"type": "text", "text": "a"}, {"type": "image"}, {"type": "text", "text": "b"}]))
                .as_deref(),
            Some("a\nb")
        );
        assert_eq!(content_text(&json!(3)), None);
    }

    #[test]
    fn a_model_switch_sets_or_clears_thinking() {
        let clear = thinking_override(None);
        assert_eq!(clear, json!({"action": "clear"}));
        let set = thinking_override(Some(
            &meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig::Adaptive,
        ));
        assert_eq!(set["action"], "set");
        assert!(set["value"]["provider_tag"].is_object(), "{set}");
    }
}
