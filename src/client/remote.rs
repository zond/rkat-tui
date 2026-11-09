//! The orchestrator member over the daemon connection: the engine's own
//! methods, with the parameters `docs/mob-split.md` settles on.
//!
//! - prompts and steers: `mob/member_send` (answered at once; the run is the
//!   member's and outlives this client);
//! - a prompt that carries a model switch: `mob/turn_start` with the
//!   `model`/`provider` overrides (request-scoped: it answers when the turn
//!   ends, and the run is cancelled if this connection closes first);
//! - interrupt: `mob/stop_member_run` for the run the stream reported, then
//!   `mob/force_cancel`;
//! - events: `mob/stream_open` (live) and `events/list_since` on the
//!   member's session (catch-up), deserialised with the engine's types.

use meerkat_contracts::{
    EventReplayScope, EventsListSinceResult, EventsSnapshotBody, EventsSnapshotResult,
};
use meerkat_core::{AgentEvent, EventEnvelope, SessionId};
use serde_json::{Value, json};

use super::ClientError;
use super::transport::{Connection, Notification};
use crate::daemon::{MOB_ID, ORCHESTRATOR};

/// The notification that carries a member stream's events.
pub const STREAM_EVENT: &str = "mob/stream_event";

/// The notification that ends a stream.
pub const STREAM_END: &str = "mob/stream_end";

/// How many replayed events one `events/list_since` page asks for.
pub const REPLAY_PAGE: usize = 500;

/// The member params every member-targeted call carries.
fn member() -> Value {
    json!({"mob_id": MOB_ID, "agent_identity": ORCHESTRATOR})
}

fn with_member(extra: Value) -> Value {
    let mut params = member();
    if let (Some(params), Value::Object(extra)) = (params.as_object_mut(), extra) {
        params.extend(extra);
    }
    params
}

fn decode<T: serde::de::DeserializeOwned>(
    what: &'static str,
    value: Value,
) -> Result<T, ClientError> {
    serde_json::from_value(value).map_err(|source| ClientError::Decode { what, source })
}

/// The orchestrator's current session.
///
/// # Errors
///
/// The call's error, or [`ClientError::NoSession`].
pub async fn session_id(connection: &Connection) -> Result<SessionId, ClientError> {
    member_session(connection, MOB_ID, ORCHESTRATOR).await
}

/// The model and provider the session runs on (`events/snapshot`).
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn identity(
    connection: &Connection,
    session: &SessionId,
) -> Result<(String, String), ClientError> {
    let snapshot = connection
        .call(
            "events/snapshot",
            json!({"scope": EventReplayScope::Session { session_id: session.clone() }}),
        )
        .await?;
    let snapshot: EventsSnapshotResult = decode("events/snapshot", snapshot)?;
    #[allow(unreachable_patterns)]
    match snapshot.snapshot {
        EventsSnapshotBody::Session { session } => Ok((session.model, session.provider)),
        _ => Err(ClientError::NoSession),
    }
}

/// Open the orchestrator's live event stream; returns its id.
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn open_stream(connection: &Connection) -> Result<String, ClientError> {
    let opened: meerkat_contracts::MobStreamOpenResult = decode(
        "mob/stream_open",
        connection.call("mob/stream_open", member()).await?,
    )?;
    Ok(opened.stream_id)
}

/// The event a `mob/stream_event` notification of `stream` carries.
#[must_use]
pub fn stream_event(
    notification: &Notification,
    stream: &str,
) -> Option<EventEnvelope<AgentEvent>> {
    if notification.method != STREAM_EVENT
        || notification.params.get("stream_id").and_then(Value::as_str) != Some(stream)
    {
        return None;
    }
    let event = notification.params.get("event")?.clone();
    match serde_json::from_value(event) {
        Ok(envelope) => Some(envelope),
        Err(err) => {
            tracing::warn!(error = %err, "a stream event this version cannot read");
            None
        }
    }
}

/// One page of the session's durable events after `after` (a sequence; 0:
/// from the start), as envelopes whose `seq` is the durable sequence; and
/// whether more follow.
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn replay(
    connection: &Connection,
    session: &SessionId,
    after: u64,
) -> Result<(Vec<EventEnvelope<AgentEvent>>, bool), ClientError> {
    let scope = EventReplayScope::Session {
        session_id: session.clone(),
    };
    let mut params = json!({"scope": scope, "limit": REPLAY_PAGE});
    if after > 0 {
        params["cursor"] = json!({"scope": scope, "sequence": after});
    }
    let page: EventsListSinceResult = decode(
        "events/list_since",
        connection.call("events/list_since", params).await?,
    )?;
    let events = page
        .events
        .into_iter()
        .map(|replayed| {
            EventEnvelope::new_session(
                session.clone(),
                replayed.cursor.sequence,
                replayed.mob_id,
                replayed.event,
            )
        })
        .collect();
    Ok((events, page.has_more))
}

/// Hand `text` to the orchestrator (`handling_mode` `queue` or `steer`).
///
/// # Errors
///
/// The call's error.
pub async fn send(connection: &Connection, text: &str, steer: bool) -> Result<(), ClientError> {
    connection
        .call(
            "mob/member_send",
            with_member(json!({
                "content": text,
                "handling_mode": if steer { "steer" } else { "queue" },
            })),
        )
        .await
        .map(drop)
}

/// A model switch: the model, its provider and the endpoint's auth binding
/// (`None`: a built-in endpoint, whose binding is cleared).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSwitch {
    /// The model id.
    pub model: String,
    /// Its provider (`anthropic`, `openai`, `gemini`).
    pub provider: Option<String>,
    /// The configured endpoint's binding, or `None` for a built-in one.
    pub auth_binding: Option<meerkat_core::AuthBindingRef>,
    /// The endpoint's name (the status bar shows it).
    pub endpoint: String,
}

/// The `mob/turn_start` params of `text` with `switch`.
#[must_use]
pub fn turn_params(text: &str, switch: &ModelSwitch) -> Value {
    let mut params = with_member(json!({"prompt": text, "model": switch.model}));
    if let Some(provider) = &switch.provider {
        params["provider"] = json!(provider);
    }
    params["auth_binding"] = match &switch.auth_binding {
        Some(binding) => json!({
            "action": "set",
            "value": meerkat_contracts::WireAuthBindingRef::from(binding.clone()),
        }),
        None => json!({"action": "clear"}),
    };
    params
}

/// Run `text` as a turn on the model of `switch`, which the session keeps
/// from then on. Answers when the turn ends.
///
/// # Errors
///
/// The call's error (an unknown model is refused before the turn starts).
pub async fn turn_with_model(
    connection: &Connection,
    text: &str,
    switch: &ModelSwitch,
) -> Result<Value, ClientError> {
    connection
        .call("mob/turn_start", turn_params(text, switch))
        .await
}

/// Stop the orchestrator's run `run_id`.
///
/// # Errors
///
/// The call's error.
pub async fn stop_run(
    connection: &Connection,
    run_id: &str,
    reason: &str,
) -> Result<Value, ClientError> {
    connection
        .call(
            "mob/stop_member_run",
            with_member(json!({"run_id": run_id, "reason": reason})),
        )
        .await
}

/// Cancel whatever the orchestrator runs.
///
/// # Errors
///
/// The call's error.
pub async fn force_cancel(connection: &Connection) -> Result<(), ClientError> {
    connection
        .call("mob/force_cancel", member())
        .await
        .map(drop)
}

/// The approval store's pending records (every requester and owner; the
/// caller filters).
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn pending_approvals(
    connection: &Connection,
) -> Result<Vec<meerkat_core::ApprovalRecord>, ClientError> {
    let listed: meerkat_contracts::ApprovalListResult = decode(
        "approval/list",
        connection
            .call("approval/list", json!({"filter": {"status": "pending"}}))
            .await?,
    )?;
    Ok(listed.approvals)
}

/// One approval record.
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn approval(
    connection: &Connection,
    id: &meerkat_core::ApprovalId,
) -> Result<meerkat_core::ApprovalRecord, ClientError> {
    decode(
        "approval/get",
        connection
            .call("approval/get", json!({"approval_id": id}))
            .await?,
    )
}

/// Decide an approval record (`approval/decide` with `params`, see
/// [`super::approvals::decide_params`]).
///
/// # Errors
///
/// The call's error: an already decided or expired record is refused.
pub async fn decide(connection: &Connection, params: Value) -> Result<(), ClientError> {
    connection.call("approval/decide", params).await.map(drop)
}

/// Call one of the daemon's own `rkat/*` methods.
///
/// # Errors
///
/// The call's error.
pub async fn local(
    connection: &Connection,
    method: &str,
    params: Value,
) -> Result<Value, ClientError> {
    connection.call(method, params).await
}

/// Append `text` to the orchestrator's system context under `key`.
///
/// # Errors
///
/// The call's error.
pub async fn append_system_context(
    connection: &Connection,
    text: &str,
    key: &str,
) -> Result<(), ClientError> {
    connection
        .call(
            "mob/append_system_context",
            with_member(json!({"text": text, "source": "rkat-tui", "idempotency_key": key})),
        )
        .await
        .map(drop)
}

/// Open the mob-wide stream of `mob` (every member's events, attributed);
/// returns its id.
///
/// # Errors
///
/// The call's error (a mob that does not exist yet is refused).
pub async fn open_mob_stream(connection: &Connection, mob: &str) -> Result<String, ClientError> {
    let opened: meerkat_contracts::MobStreamOpenResult = decode(
        "mob/stream_open",
        connection
            .call("mob/stream_open", json!({"mob_id": mob}))
            .await?,
    )?;
    Ok(opened.stream_id)
}

/// Close a stream.
///
/// # Errors
///
/// The call's error.
pub async fn close_stream(connection: &Connection, stream: &str) -> Result<(), ClientError> {
    connection
        .call("mob/stream_close", json!({"stream_id": stream}))
        .await
        .map(drop)
}

/// The orchestrator's live stream, opened again (after a conversation
/// switch the old one still follows the retired session).
///
/// # Errors
///
/// The call's error.
pub async fn reopen_stream(connection: &Connection, old: &str) -> Result<String, ClientError> {
    if let Err(err) = close_stream(connection, old).await {
        tracing::debug!(error = %err, "the old stream did not close");
    }
    open_stream(connection).await
}

/// The mobs the daemon hosts.
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn mobs(connection: &Connection) -> Result<Vec<String>, ClientError> {
    let listed: meerkat_contracts::MobListResult =
        decode("mob/list", connection.call("mob/list", json!({})).await?)?;
    Ok(listed.mobs.into_iter().map(|mob| mob.mob_id).collect())
}

/// The member identities of `mob`.
///
/// # Errors
///
/// The call's error.
pub async fn members(connection: &Connection, mob: &str) -> Result<Vec<String>, ClientError> {
    let listed = connection
        .call("mob/members", json!({"mob_id": mob}))
        .await?;
    Ok(listed
        .get("members")
        .and_then(Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(|member| member.get("agent_identity").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// The session `identity` of `mob` runs as.
///
/// # Errors
///
/// The call's error, or [`ClientError::NoSession`].
pub async fn member_session(
    connection: &Connection,
    mob: &str,
    identity: &str,
) -> Result<SessionId, ClientError> {
    let status = connection
        .call(
            "mob/member_status",
            json!({"mob_id": mob, "agent_identity": identity}),
        )
        .await?;
    status
        .get("current_session_id")
        .and_then(Value::as_str)
        .and_then(|id| SessionId::parse(id).ok())
        .ok_or(ClientError::NoSession)
}

/// The attributed event a mob-wide stream's notification carries, and the
/// stream's id.
#[must_use]
pub fn attributed_event(
    notification: &Notification,
) -> Option<(String, meerkat_mob::AttributedEvent)> {
    if notification.method != STREAM_EVENT {
        return None;
    }
    let stream = notification.params.get("stream_id")?.as_str()?.to_string();
    let event = notification.params.get("event")?.clone();
    match serde_json::from_value(event) {
        Ok(event) => Some((stream, event)),
        Err(err) => {
            tracing::debug!(error = %err, "not an attributed event");
            None
        }
    }
}

/// The stream a `mob/stream_end` notification ends.
#[must_use]
pub fn stream_end(notification: &Notification) -> Option<&str> {
    (notification.method == STREAM_END)
        .then(|| notification.params.get("stream_id")?.as_str())
        .flatten()
}

/// The orchestrator's conversations (`rkat/conversations`).
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn conversations(
    connection: &Connection,
) -> Result<Vec<crate::daemon::conversations::Listed>, ClientError> {
    let answer = local(connection, crate::daemon::local::CONVERSATIONS, json!({})).await?;
    decode(
        "rkat/conversations",
        answer.get("conversations").cloned().unwrap_or(Value::Null),
    )
}

/// `/new` (`session: None`) or `/resume` (`rkat/new`, `rkat/resume`): the
/// session the orchestrator runs as afterwards, and a warning.
///
/// # Errors
///
/// The call's error (the daemon refused or failed the switch).
pub async fn switch(
    connection: &Connection,
    session: Option<&SessionId>,
) -> Result<(SessionId, Option<String>), ClientError> {
    let answer = match session {
        None => local(connection, crate::daemon::local::NEW, json!({})).await?,
        Some(session) => {
            local(
                connection,
                crate::daemon::local::RESUME,
                json!({"session_id": session}),
            )
            .await?
        }
    };
    let session = answer
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(|id| SessionId::parse(id).ok())
        .ok_or(ClientError::NoSession)?;
    let warning = answer
        .get("warning")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok((session, warning))
}

/// A conversation's committed messages (`rkat/transcript`).
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn transcript(
    connection: &Connection,
    session: &SessionId,
) -> Result<Vec<meerkat_core::Message>, ClientError> {
    let answer = local(
        connection,
        crate::daemon::local::TRANSCRIPT,
        json!({"session_id": session}),
    )
    .await?;
    decode(
        "rkat/transcript",
        answer.get("messages").cloned().unwrap_or(Value::Null),
    )
}

/// A conversation's total token usage, `(input, output)` (`rkat/usage`).
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn usage(
    connection: &Connection,
    session: &SessionId,
) -> Result<(u64, u64), ClientError> {
    #[derive(serde::Deserialize)]
    struct Totals {
        input_tokens: u64,
        output_tokens: u64,
    }
    let answer = local(
        connection,
        crate::daemon::local::USAGE,
        json!({"session_id": session}),
    )
    .await?;
    let totals: Totals = decode("rkat/usage", answer)?;
    Ok((totals.input_tokens, totals.output_tokens))
}

/// `/compact now` for the orchestrator: `mob/compact_member` (its next
/// compaction check compacts). Returns the engine's refusal as text.
///
/// # Errors
///
/// The call's error, as text.
pub async fn compact_member(connection: &Connection) -> Result<(), String> {
    connection
        .call("mob/compact_member", member())
        .await
        .map(drop)
        .map_err(|err| err.to_string())
}

/// `/compact now` through the orchestrator's session (`session/compact`).
///
/// # Errors
///
/// The call's error, as text.
pub async fn compact_session(connection: &Connection, session: &SessionId) -> Result<(), String> {
    connection
        .call("session/compact", json!({"session_id": session}))
        .await
        .map(drop)
        .map_err(|err| err.to_string())
}
