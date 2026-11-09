//! The daemon's wire, typed once for both ends: the method names (the
//! daemon's own `rkat/*` ones and the engine's the client calls and the
//! daemon observes), the JSON-RPC error codes, and the parameters and
//! results of every `rkat/*` method, of `decision/decide` as a client sends
//! it and of the `rkat/decision_ended` notification.
//!
//! [`super::local`] answers the `rkat/*` methods, [`super::handshake`] the
//! attach line, [`super::decisions`] `decision/decide`; the client
//! (`crate::client::{remote, runner, approvals}`) calls them with these
//! types. `tests/daemon.rs` tests the raw wire below them.

use meerkat_contracts::WireDecisionPhase;
use meerkat_core::{Message, SessionId};
use meerkat_rpc::protocol::{RpcId, RpcResponse};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::conversations::Listed;
use crate::approval::desk::{ModeStatus, RemoteAnswer};

pub use meerkat_rpc::error::{INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND};

/// Every local method starts with this.
pub const PREFIX: &str = "rkat/";
/// The attach handshake ([`super::handshake`]): [`AttachParams`] →
/// [`Attached`].
pub const ATTACH: &str = "rkat/attach";
/// `{}` → [`ModeStatus`]: the permission mode and the denial counters.
pub const GET_MODE: &str = "rkat/get_mode";
/// [`SetModeParams`] → [`ModeSet`]: switch the permission mode.
pub const SET_MODE: &str = "rkat/set_mode";
/// A [`crate::approval::desk::RuleCommand`] →
/// [`crate::approval::desk::RuleOutcome`]: a `/rules` command.
pub const RULES: &str = "rkat/rules";
/// `{}` → [`ConversationList`]: the orchestrator's conversations.
pub const CONVERSATIONS: &str = "rkat/conversations";
/// `{}` → [`Switched`]: `/new`.
pub const NEW: &str = "rkat/new";
/// [`SessionParams`] → [`Switched`]: `/resume <session>`.
pub const RESUME: &str = "rkat/resume";
/// [`SessionParams`] → [`Transcript`]: a conversation's committed messages.
pub const TRANSCRIPT: &str = "rkat/transcript";
/// [`SessionParams`] → [`Usage`]: a conversation's total token usage.
pub const USAGE: &str = "rkat/usage";
/// [`SessionParams`] → [`SessionEndpoint`]: the configured endpoint a
/// conversation's auth binding names (the status bar shows it, as the
/// in-process runner derived it from the session's binding).
pub const ENDPOINT: &str = "rkat/endpoint";
/// [`TrustParams`] → [`crate::approval::desk::RuleOutcome`]: the project
/// trust record (`/trust`, `/rules trust`).
pub const TRUST: &str = "rkat/trust";
/// [`SessionParams`] → [`Tasks`]: a conversation's tasks in the engine's
/// task store (the Ctrl+T checklist).
pub const TASKS: &str = "rkat/tasks";
/// [`ChecklistViewParams`] → `{}`: record the checklist panel's view
/// (Ctrl+T) as the realm's `[ui] checklist`.
pub const CHECKLIST_VIEW: &str = "rkat/checklist_view";
/// [`SetModelParams`] → [`ModelSet`]: switch the orchestrator's current
/// session to another model in place (`/model`), with the endpoint's auth
/// binding and the `[thinking]` setting the model is configured with.
pub const SET_MODEL: &str = "rkat/set_model";
/// The notification that a decision a client was shown ended:
/// [`DecisionEnded`].
pub const DECISION_ENDED: &str = "rkat/decision_ended";
/// `{}` → [`Notices`]: what the daemon's start-up had to say (the
/// confinement, the rules, the configuration), as the in-process TUI shows
/// it at start.
pub const NOTICES: &str = "rkat/notices";
/// The notification of a change the orchestrator's file tools made:
/// [`FileDiffNote`].
pub const FILE_DIFF: &str = "rkat/file_diff";
/// `{}` → [`Status`]: the daemon's lifecycle state as the caller sees it
/// (`/quit` decides from it, `--list` prints it).
pub const STATUS: &str = "rkat/status";
/// `{}` → `{}`: the caller leaves and wants the agent kept running
/// (`/detach`); the daemon records that it has been detached from.
pub const DETACH: &str = "rkat/detach";
/// `{}` → `{}`: stop the daemon (`/quit` when this client started it and
/// nobody detached since, or the choice "terminate"): it removes its
/// endpoint file and shuts down as on SIGTERM.
pub const SHUTDOWN: &str = "rkat/shutdown";
/// The engine's decision notice, pushed by the daemon.
pub const DECISION_NOTICE: &str = meerkat_contracts::DECISION_NOTICE_NOTIFICATION;
/// The engine's decide method, decided by the daemon ([`DecideParams`]).
pub const DECISION_DECIDE: &str = meerkat_rpc::router::DECISION_DECIDE_METHOD;

/// The engine's methods the client calls (and the daemon observes on their
/// way to the router).
pub mod engine {
    /// A prompt or a steer for a member.
    pub const MEMBER_SEND: &str = "mob/member_send";
    /// A turn with request-scoped overrides (a model switch).
    pub const TURN_START: &str = "mob/turn_start";
    /// Stop a member's run.
    pub const STOP_MEMBER_RUN: &str = "mob/stop_member_run";
    /// Cancel whatever a member runs.
    pub const FORCE_CANCEL: &str = "mob/force_cancel";
    /// Open a member's or a mob's event stream.
    pub const STREAM_OPEN: &str = "mob/stream_open";
    /// Close a stream.
    pub const STREAM_CLOSE: &str = "mob/stream_close";
    /// A stream's event (a notification).
    pub const STREAM_EVENT: &str = "mob/stream_event";
    /// A stream's end (a notification).
    pub const STREAM_END: &str = "mob/stream_end";
    /// Append to a member's system context.
    pub const APPEND_SYSTEM_CONTEXT: &str = "mob/append_system_context";
    /// The mobs a runtime hosts.
    pub const LIST: &str = "mob/list";
    /// A mob's members.
    pub const MEMBERS: &str = "mob/members";
    /// A member's status (its current session).
    pub const MEMBER_STATUS: &str = "mob/member_status";
    /// Compact a member at its next compaction check.
    pub const COMPACT_MEMBER: &str = "mob/compact_member";
    /// Compact a session.
    pub const SESSION_COMPACT: &str = "session/compact";
    /// A session's durable events after a cursor.
    pub const EVENTS_LIST_SINCE: &str = "events/list_since";
    /// A session's snapshot and its replay cursor.
    pub const EVENTS_SNAPSHOT: &str = "events/snapshot";
    /// The capability handshake.
    pub const INITIALIZE: &str = "initialize";
}

/// The attach token is missing or wrong, or no attach arrived in time.
pub const UNAUTHORIZED: i32 = -32001;
/// A conversation switch or read the engine refused.
pub const SWITCH_FAILED: i32 = -32010;
/// A conversation switch refused because the orchestrator's turn is
/// running.
pub const SWITCH_BUSY: i32 = -32011;

/// A local method's refusal: a JSON-RPC error code and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The JSON-RPC error code.
    pub code: i32,
    /// Its message.
    pub message: String,
}

impl Refusal {
    /// A refusal with `code`.
    #[must_use]
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// [`INVALID_PARAMS`] for parameters that do not read as `T`.
    #[must_use]
    pub fn invalid(err: &serde_json::Error) -> Self {
        Self::new(INVALID_PARAMS, err.to_string())
    }
}

/// Read a method's parameters (`null`: none, read as `{}`).
///
/// # Errors
///
/// [`INVALID_PARAMS`] when they do not read as `T`.
pub fn params<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, Refusal> {
    let params = if params.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        params
    };
    serde_json::from_value(params).map_err(|err| Refusal::invalid(&err))
}

/// A request id as the engine's wire types it (`None`: absent, `null`, or
/// not a number or string).
#[must_use]
pub fn request_id(id: &Value) -> Option<RpcId> {
    serde_json::from_value(id.clone()).ok()
}

/// The answer line (newline included) of the request `id`: its result.
#[must_use]
pub fn result_line(id: Option<RpcId>, result: impl Serialize) -> String {
    response_line(&RpcResponse::success(id, result))
}

/// The answer line (newline included) of the request `id`: a refusal.
#[must_use]
pub fn error_line(id: Option<RpcId>, code: i32, message: &str) -> String {
    response_line(&RpcResponse::error(id, code, message))
}

/// `response` as a line, newline included.
#[must_use]
pub fn response_line(response: &RpcResponse) -> String {
    let mut line = serde_json::to_string(response).unwrap_or_else(|err| {
        format!(
            r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":{INTERNAL_ERROR},"message":"cannot write the answer: {err}"}}}}"#
        )
    });
    line.push('\n');
    line
}

/// [`ATTACH`]'s parameters.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachParams {
    /// The endpoint file's token.
    pub token: String,
    /// The start token the client that launched the daemon handed it
    /// (`RKAT_TUI_START_TOKEN`): the daemon counts the connection that
    /// presents it as its owner ([`Status::owner`]). A client presents its
    /// own on every attach; a daemon it did not start ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_token: Option<String>,
    /// A connection that only reads the status (`--list`): not counted as
    /// an attached client, and its end is no detach.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub probe: bool,
}

impl std::fmt::Debug for AttachParams {
    /// Never the tokens.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachParams")
            .field("start_token", &self.start_token.as_ref().map(|_| "…"))
            .field("probe", &self.probe)
            .finish_non_exhaustive()
    }
}

/// [`ATTACH`]'s result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attached {
    /// Always `true`.
    pub attached: bool,
}

/// [`STATUS`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The canonical workspace path the daemon serves.
    pub workspace: std::path::PathBuf,
    /// The host name of the machine the daemon runs on (`None`: an older
    /// daemon, or one that could not read it). A client reached through a
    /// tunnel compares it with its own to tell whether the workspace is
    /// on its machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The daemon's process id.
    pub pid: u32,
    /// When it started, RFC 3339 in UTC.
    pub started_at: String,
    /// The attached clients, the caller included (probes are not counted).
    pub clients: usize,
    /// Whether any client has detached from this daemon since it started
    /// (`/detach`, or its owner's connection ending without
    /// [`SHUTDOWN`]); never reset.
    pub detached: bool,
    /// Whether the caller is the client that started the daemon.
    pub owner: bool,
    /// The orchestrator's current conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// Whether the orchestrator's turn is running (`false` when its state
    /// cannot be read).
    pub running: bool,
}

/// [`SET_MODE`]'s parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetModeParams {
    /// `default`, `accept-edits`, `plan` or `auto` (case-insensitive).
    pub mode: String,
}

/// [`SET_MODE`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeSet {
    /// The mode and the counters now.
    pub status: ModeStatus,
    /// What the mode does.
    pub notice: String,
    /// What did not work (not saved, the agent not told).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// [`CONVERSATIONS`]'s result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationList {
    /// Last used first.
    pub conversations: Vec<Listed>,
}

/// The parameters of the methods on one conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionParams {
    /// The conversation's session.
    pub session_id: SessionId,
}

/// [`NEW`]'s and [`RESUME`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Switched {
    /// The session the orchestrator runs as now.
    pub session_id: SessionId,
    /// What did not work (the new conversation not told about plan mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// [`TRANSCRIPT`]'s result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    /// The committed messages.
    pub messages: Vec<Message>,
}

/// A notification the daemon pushes to every attached client with what the
/// approval engine reports ([`ApprovalNote`]): the classifier's verdicts,
/// auto mode pausing and resuming, the classifier's work and token usage.
pub const APPROVAL_NOTE: &str = "rkat/approval_note";

/// What the approval engine reported, as [`APPROVAL_NOTE`] carries it (the
/// engine's `ApprovalEvent` without the dialogs, which the consent surface
/// pushes as decisions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalNote {
    /// A tool call was decided.
    Decision {
        /// The helper that made the call (`None`: the orchestrator).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        /// Its tool-use id.
        tool_use_id: String,
        /// The tool.
        tool: String,
        /// The tier and who decided.
        verdict: crate::approval::policy::VerdictSummary,
        /// Why it was denied, or the classifier's reason for an allow.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Auto mode paused.
    Paused {
        /// Why.
        cause: crate::approval::counters::PauseCause,
    },
    /// Auto mode resumed.
    Resumed,
    /// Broad allow rules were dropped on entering auto mode (described).
    RulesDropped {
        /// The rules, as `/rules` lists them.
        rules: Vec<String>,
    },
    /// The classifier started or finished a review.
    Classifying {
        /// Whether one runs now.
        running: bool,
    },
    /// One classifier call's token usage.
    ClassifierUsage {
        /// The classifier's model.
        model: String,
        /// Its usage.
        usage: meerkat_core::Usage,
    },
    /// Something the human should know about the approval machinery.
    Notice {
        /// What.
        text: String,
    },
}

impl ApprovalNote {
    /// The note for `event` (`None`: a withdrawn escalation, which the
    /// decision push already closes).
    #[must_use]
    pub fn of(event: crate::approval::ApprovalEvent) -> Option<Self> {
        use crate::approval::ApprovalEvent;
        Some(match event {
            ApprovalEvent::Decision {
                agent,
                tool_use_id,
                tool,
                verdict,
                reason,
                ..
            } => Self::Decision {
                agent,
                tool_use_id,
                tool,
                verdict,
                reason,
            },
            ApprovalEvent::Paused(cause) => Self::Paused { cause },
            ApprovalEvent::Resumed => Self::Resumed,
            ApprovalEvent::RulesDropped(rules) => Self::RulesDropped {
                rules: rules
                    .iter()
                    .map(crate::approval::desk::describe_rule)
                    .collect(),
            },
            ApprovalEvent::Classifying(running) => Self::Classifying { running },
            ApprovalEvent::ClassifierUsage { model, usage } => {
                Self::ClassifierUsage { model, usage }
            }
            ApprovalEvent::Notice(text) => Self::Notice { text },
            ApprovalEvent::EscalationWithdrawn(_) => return None,
        })
    }
}

/// [`ENDPOINT`]'s result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEndpoint {
    /// The configured endpoint (`None`: a built-in one, named by the
    /// provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// [`USAGE`]'s result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
}

/// [`TASKS`]'s result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tasks {
    /// The conversation's tasks as the store lists them.
    pub tasks: Vec<meerkat_tools::builtin::types::Task>,
}

/// [`CHECKLIST_VIEW`]'s parameters.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ChecklistViewParams {
    /// `hidden` or `shown`.
    pub view: crate::ui::ChecklistView,
}

/// [`SET_MODEL`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetModelParams {
    /// The model id.
    pub model: String,
    /// Its provider (`anthropic`, `openai`, `gemini`); `None` keeps the
    /// session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The configured endpoint's binding; `None` (a built-in endpoint)
    /// clears the session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_binding: Option<meerkat_core::AuthBindingRef>,
    /// What the endpoint's model list says of the model (a model only an
    /// endpoint knows gets a profile in the realm config from it, as the
    /// in-process switch registered one); `None`: not listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed: Option<ListedModel>,
}

/// A model as the client's endpoint lists report it ([`SetModelParams`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedModel {
    /// Its context window in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Its output limit in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

/// [`SET_MODEL`]'s result: the session's identity after the switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSet {
    /// The session the switch applied to.
    pub session_id: SessionId,
    /// Its model now.
    pub model: String,
    /// Its provider now.
    pub provider: String,
    /// The orchestrator was seated again on the session first (its agent
    /// rebuilt for a model it had no profile for): the conversation's
    /// stream is a new one, which the client opens again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reseated: bool,
}

/// What [`TRUST`] acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustOp {
    /// `/trust`: the project inputs.
    Project,
    /// `/rules trust`: the realm rules file.
    Rules,
}

/// [`TRUST`]'s parameters.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TrustParams {
    /// What to trust.
    pub op: TrustOp,
}

/// `decision/decide` as a client of the daemon sends it: the engine's
/// closed request, and the dialog's answer (stripped before dispatch).
/// Closed: a field that names who decided (`actor`, `approver`, …) is
/// refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecideParams {
    /// The decision.
    pub decision_id: String,
    /// Approved (else rejected).
    pub approve: bool,
    /// The digest of the rendering the client showed.
    pub presented_digest: String,
    /// The decider's reason (the dialog's comment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The dialog's answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<RemoteAnswer>,
}

/// One start-up notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupNotice {
    /// A warning (else information).
    pub warn: bool,
    /// What it says.
    pub text: String,
}

impl StartupNotice {
    /// Information.
    #[must_use]
    pub fn info(text: String) -> Self {
        Self { warn: false, text }
    }

    /// A warning.
    #[must_use]
    pub fn warn(text: String) -> Self {
        Self { warn: true, text }
    }
}

/// [`NOTICES`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notices {
    /// In the order the in-process TUI shows them.
    pub notices: Vec<StartupNotice>,
}

/// [`FILE_DIFF`]'s parameters: one file change, as the in-process file
/// tools report it to the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiffNote {
    /// The tool call that made the change.
    pub tool_use_id: String,
    /// The file (relative to the workspace when inside it).
    pub path: String,
    /// The unified diff.
    pub unified: String,
    /// Lines added.
    pub added: usize,
    /// Lines removed.
    pub removed: usize,
}

/// [`DECISION_ENDED`]'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionEnded {
    /// The decision.
    pub decision_id: String,
    /// The phase it ended in, when known.
    pub phase: Option<WireDecisionPhase>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn answer_lines_are_json_rpc_responses() {
        let ok = result_line(
            Some(RpcId::Num(3)),
            Usage {
                input_tokens: 1,
                output_tokens: 2,
            },
        );
        let ok: Value = serde_json::from_str(ok.trim_end()).unwrap();
        assert_eq!(
            ok,
            json!({"jsonrpc": "2.0", "id": 3, "result": {"input_tokens": 1, "output_tokens": 2}})
        );
        let err = error_line(request_id(&json!("a")), SWITCH_BUSY, "busy");
        let err: Value = serde_json::from_str(err.trim_end()).unwrap();
        assert_eq!(
            err,
            json!({"jsonrpc": "2.0", "id": "a", "error": {"code": SWITCH_BUSY, "message": "busy"}})
        );
        assert_eq!(request_id(&Value::Null), None);
    }

    #[test]
    fn a_decide_naming_who_decided_is_refused_and_no_params_read_as_empty() {
        assert!(params::<DecideParams>(json!({"decision_id": "d", "approve": true, "presented_digest": "x", "actor": "mallory"})).is_err());
        assert!(params::<SetModeParams>(Value::Null).is_err());
        let trust = params::<TrustParams>(json!({"op": "rules"})).unwrap();
        assert_eq!(trust.op, TrustOp::Rules);
        let refused = params::<TrustParams>(json!({})).unwrap_err();
        assert_eq!(refused.code, INVALID_PARAMS);
    }
}
