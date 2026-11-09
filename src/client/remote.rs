//! The orchestrator member over the daemon connection: the engine's own
//! methods, with the parameters `docs/mob-split.md` settles on.
//!
//! - prompts and steers: `mob/member_send` (answered at once; the run is the
//!   member's and outlives this client);
//! - a model switch: the daemon's `rkat/set_model`, in place on the
//!   member's session (`mob/turn_start` is not used: its run is tied to the
//!   connection that sent it);
//! - interrupt: `mob/stop_member_run` for the run the stream reported, then
//!   `mob/force_cancel`;
//! - events: `mob/stream_open`, resumed after the last event shown with its
//!   `{epoch, seq}` cursor; `events/list_since` on the member's session only
//!   to render a conversation from its start (its last sequence and gauge)
//!   and for a helper's backfill; deserialised with the engine's types.

use meerkat_contracts::{
    EventReplayScope, EventsListSinceResult, EventsSnapshotBody, EventsSnapshotResult,
};
use meerkat_core::{AgentEvent, EventEnvelope, SessionId};
use serde_json::{Value, json};

use super::ClientError;
use super::transport::{Connection, Notification};
use crate::daemon::protocol::{
    self, ConversationList, SessionParams, Switched, Transcript, Usage, engine,
};
use crate::daemon::{MOB_ID, ORCHESTRATOR};

/// The notification that carries a member stream's events.
pub const STREAM_EVENT: &str = engine::STREAM_EVENT;

/// The notification that ends a stream.
pub const STREAM_END: &str = engine::STREAM_END;

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
    snapshot(connection, session)
        .await
        .map(|snapshot| (snapshot.model, snapshot.provider))
}

/// The configured endpoint `session`'s auth binding names (`rkat/endpoint`;
/// `None`: a built-in endpoint).
///
/// # Errors
///
/// The call's error (an older daemon has no such method).
pub async fn session_endpoint(
    connection: &Connection,
    session: &SessionId,
) -> Result<Option<String>, ClientError> {
    let answer: crate::daemon::protocol::SessionEndpoint = local(
        connection,
        protocol::ENDPOINT,
        &SessionParams {
            session_id: session.clone(),
        },
    )
    .await?;
    Ok(answer.endpoint)
}

/// What `events/snapshot` says of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The model it runs on.
    pub model: String,
    /// Its provider.
    pub provider: String,
    /// The sequence of its last durable event (0: none), paired with the
    /// snapshot.
    pub latest: u64,
}

/// The session's snapshot (`events/snapshot`): its model and provider and
/// its latest durable sequence.
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn snapshot(
    connection: &Connection,
    session: &SessionId,
) -> Result<Snapshot, ClientError> {
    let snapshot = connection
        .call(
            engine::EVENTS_SNAPSHOT,
            json!({"scope": EventReplayScope::Session { session_id: session.clone() }}),
        )
        .await?;
    let snapshot: EventsSnapshotResult = decode("events/snapshot", snapshot)?;
    let latest = snapshot.cursor.sequence;
    #[allow(unreachable_patterns)]
    match snapshot.snapshot {
        EventsSnapshotBody::Session { session } => Ok(Snapshot {
            model: session.model,
            provider: session.provider,
            latest,
        }),
        _ => Err(ClientError::NoSession),
    }
}

/// Open the orchestrator's event stream; returns its id. Live, or with
/// `cursor` (`(epoch, seq)` of the last event shown) the events the member
/// still retains after it first: of another epoch, from the earliest one
/// the member retains.
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn open_stream(
    connection: &Connection,
    cursor: Option<(&str, u64)>,
) -> Result<String, ClientError> {
    let mut params = member();
    if let Some((epoch, seq)) = cursor {
        params["cursor"] = json!(meerkat_contracts::MobStreamCursor {
            epoch: epoch.to_string(),
            seq,
        });
    }
    let opened: meerkat_contracts::MobStreamOpenResult = decode(
        engine::STREAM_OPEN,
        connection.call(engine::STREAM_OPEN, params).await?,
    )?;
    Ok(opened.stream_id)
}

/// The event a `mob/stream_event` notification of `stream` carries, and
/// the sequence space (`epoch`) of its `seq`.
#[must_use]
pub fn stream_event(
    notification: &Notification,
    stream: &str,
) -> Option<(EventEnvelope<AgentEvent>, Option<String>)> {
    if notification.method != STREAM_EVENT
        || notification.params.get("stream_id").and_then(Value::as_str) != Some(stream)
    {
        return None;
    }
    let event = notification.params.get("event")?.clone();
    let epoch = notification
        .params
        .get("epoch")
        .and_then(Value::as_str)
        .map(str::to_string);
    match serde_json::from_value(event) {
        Ok(envelope) => Some((envelope, epoch)),
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
        engine::EVENTS_LIST_SINCE,
        connection.call(engine::EVENTS_LIST_SINCE, params).await?,
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
            engine::MEMBER_SEND,
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
    /// What the endpoint's list says of the model (`None`: not listed).
    pub listed: Option<crate::daemon::protocol::ListedModel>,
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
            engine::STOP_MEMBER_RUN,
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
        .call(engine::FORCE_CANCEL, member())
        .await
        .map(drop)
}

/// Call one of the daemon's own `rkat/*` methods (`method`, one of
/// [`protocol`]'s) with `params`, its answer read as `R`.
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn local<P: serde::Serialize, R: serde::de::DeserializeOwned>(
    connection: &Connection,
    method: &'static str,
    params: &P,
) -> Result<R, ClientError> {
    let params = serde_json::to_value(params).map_err(|source| ClientError::Decode {
        what: method,
        source,
    })?;
    decode(method, connection.call(method, params).await?)
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
            engine::APPEND_SYSTEM_CONTEXT,
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
        engine::STREAM_OPEN,
        connection
            .call(engine::STREAM_OPEN, json!({"mob_id": mob}))
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
        .call(engine::STREAM_CLOSE, json!({"stream_id": stream}))
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
    open_stream(connection, None).await
}

/// The mobs the daemon hosts.
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn mobs(connection: &Connection) -> Result<Vec<String>, ClientError> {
    let listed: meerkat_contracts::MobListResult = decode(
        engine::LIST,
        connection.call(engine::LIST, json!({})).await?,
    )?;
    Ok(listed.mobs.into_iter().map(|mob| mob.mob_id).collect())
}

/// The member identities of `mob`.
///
/// # Errors
///
/// The call's error.
pub async fn members(connection: &Connection, mob: &str) -> Result<Vec<String>, ClientError> {
    let listed = connection
        .call(engine::MEMBERS, json!({"mob_id": mob}))
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
            engine::MEMBER_STATUS,
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
    let listed: ConversationList = local(connection, protocol::CONVERSATIONS, &json!({})).await?;
    Ok(listed.conversations)
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
    let switched: Switched = match session {
        None => local(connection, protocol::NEW, &json!({})).await?,
        Some(session) => {
            local(
                connection,
                protocol::RESUME,
                &SessionParams {
                    session_id: session.clone(),
                },
            )
            .await?
        }
    };
    Ok((switched.session_id, switched.warning))
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
    let transcript: Transcript = local(
        connection,
        protocol::TRANSCRIPT,
        &SessionParams {
            session_id: session.clone(),
        },
    )
    .await?;
    Ok(transcript.messages)
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
    let totals: Usage = local(
        connection,
        protocol::USAGE,
        &SessionParams {
            session_id: session.clone(),
        },
    )
    .await?;
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
        .call(engine::COMPACT_MEMBER, member())
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
        .call(engine::SESSION_COMPACT, json!({"session_id": session}))
        .await
        .map(drop)
        .map_err(|err| err.to_string())
}
