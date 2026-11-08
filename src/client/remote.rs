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
    let status = connection.call("mob/member_status", member()).await?;
    let id = status
        .get("current_session_id")
        .and_then(Value::as_str)
        .ok_or(ClientError::NoSession)?;
    SessionId::parse(id).map_err(|_| ClientError::NoSession)
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

/// Run `text` as a turn on `model` (and `provider`), which the session keeps
/// from then on. Answers when the turn ends.
///
/// # Errors
///
/// The call's error (an unknown model is refused before the turn starts).
pub async fn turn_with_model(
    connection: &Connection,
    text: &str,
    model: &str,
    provider: Option<&str>,
) -> Result<Value, ClientError> {
    let mut params = with_member(json!({"prompt": text, "model": model}));
    if let Some(provider) = provider {
        params["provider"] = json!(provider);
    }
    connection.call("mob/turn_start", params).await
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
