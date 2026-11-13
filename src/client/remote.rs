//! The agent member over the daemon connection: the engine's own
//! methods, with the parameters `docs/mob-split.md` settles on.
//!
//! - prompts and steers: `mob/member_send` (answered at once; the run is the
//!   member's and outlives this client);
//! - a model switch: the daemon's `rkat/set_model`, in place on the
//!   member's session (the daemon does not serve `mob/turn_start`, whose
//!   run is tied to the connection that sent it);
//! - interrupt: `mob/stop_member_run` for the run the stream reported, then
//!   `mob/force_cancel`;
//! - events: `mob/stream_open`, resumed after the last event shown with its
//!   `{epoch, seq}` cursor; `events/list_since` on the member's session only
//!   to render a conversation from its start (its last sequence and gauge)
//!   and for a helper's backfill; deserialised with the engine's types.

use meerkat_contracts::{
    EventReplayScope, EventsListSinceResult, EventsSnapshotBody, EventsSnapshotResult,
    MobMemberSendResult,
};
use meerkat_core::lifecycle::InputId;
use meerkat_core::{AgentEvent, EventEnvelope, SessionId};
use serde_json::{Value, json};

use super::ClientError;
use super::transport::Connection;
use crate::daemon::MOB_ID;
use crate::daemon::protocol::{Empty, Method, SessionParams, engine, method};

/// The notification that carries a member stream's events.
pub const STREAM_EVENT: &str = engine::STREAM_EVENT;

/// The notification that ends a stream.
pub const STREAM_END: &str = engine::STREAM_END;

/// How many replayed events one `events/list_since` page asks for.
pub const REPLAY_PAGE: usize = 500;

/// The member params every member-targeted call carries: the agent the
/// connection is bound to.
fn member(connection: &Connection) -> Value {
    json!({"mob_id": MOB_ID, "agent_identity": connection.agent()})
}

fn with_member(connection: &Connection, extra: Value) -> Value {
    let mut params = member(connection);
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
        EventsSnapshotBody::Session { session } => {
            // A self-hosted alias is the model on its endpoint.
            let (model, provider) =
                crate::endpoints::from_engine(&session.model, &session.provider);
            Ok(Snapshot {
                model,
                provider,
                latest,
            })
        }
        _ => Err(ClientError::NoSession),
    }
}

/// Open the agent's event stream; returns its id. Live, or with
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
    let mut params = member(connection);
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

/// Hand `text` to the agent (`handling_mode` `queue` or `steer`):
/// the input its runtime admitted (`admission.input_id`), which the
/// agent's `boundary_append_applied` names when a steer joins the run.
/// `None` when the daemon named none (a member not served by a local
/// runtime: the agent always is).
///
/// # Errors
///
/// The call's error, [`ClientError::Decode`] for an answer that is not a
/// `MobMemberSendResult`, or [`ClientError::Protocol`] for an input id
/// that is not one.
pub async fn send(
    connection: &Connection,
    text: &str,
    steer: bool,
) -> Result<Option<InputId>, ClientError> {
    let answer = connection
        .call(
            engine::MEMBER_SEND,
            with_member(
                connection,
                json!({
                    "content": text,
                    "handling_mode": if steer { "steer" } else { "queue" },
                }),
            ),
        )
        .await?;
    let answer: MobMemberSendResult = decode("mob/member_send", answer)?;
    answer
        .admission
        .map(|admission| {
            uuid::Uuid::parse_str(&admission.input_id)
                .map(InputId::from_uuid)
                .map_err(|_| {
                    ClientError::Protocol(format!(
                        "mob/member_send named the input `{}`, which is not an input id",
                        admission.input_id
                    ))
                })
        })
        .transpose()
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

/// Stop the agent's run `run_id`.
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
            with_member(connection, json!({"run_id": run_id, "reason": reason})),
        )
        .await
}

/// Interrupt the agent's current run at once, whichever it is (queued
/// input stays queued).
///
/// # Errors
///
/// The call's error.
pub async fn hard_cancel(connection: &Connection, reason: &str) -> Result<(), ClientError> {
    connection
        .call(
            engine::HARD_CANCEL_MEMBER,
            with_member(connection, json!({"reason": reason})),
        )
        .await
        .map(drop)
}

/// Whether the answer of [`stop_run`] says the run was stopped (its
/// receipt's `outcome` is `stopped`; `not_current` and `not_stoppable`
/// stopped nothing).
#[must_use]
pub fn stopped(answer: &Value) -> bool {
    answer
        .pointer("/receipt/outcome")
        .and_then(Value::as_str)
        .is_some_and(|outcome| outcome == "stopped")
}

/// Cancel whatever the agent runs.
///
/// # Errors
///
/// The call's error.
pub async fn force_cancel(connection: &Connection) -> Result<(), ClientError> {
    connection
        .call(engine::FORCE_CANCEL, member(connection))
        .await
        .map(drop)
}

/// Call the daemon's own method `M` with `params`, its answer read as
/// `M`'s result ([`Method`]).
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn call<M: Method>(
    connection: &Connection,
    params: &M::Params,
) -> Result<M::Result, ClientError> {
    let params = serde_json::to_value(params).map_err(|source| ClientError::Decode {
        what: M::NAME,
        source,
    })?;
    decode(M::NAME, connection.call(M::NAME, params).await?)
}

/// `decision/decide` with `params`: the daemon decides it (its answer is
/// the engine's decision record, which the client does not read).
///
/// # Errors
///
/// The call's error (a decision already settled is refused).
pub async fn decide(
    connection: &Connection,
    params: &crate::daemon::protocol::DecideParams,
) -> Result<Value, ClientError> {
    let params = serde_json::to_value(params).map_err(|source| ClientError::Decode {
        what: crate::daemon::protocol::DECISION_DECIDE,
        source,
    })?;
    connection
        .call(crate::daemon::protocol::DECISION_DECIDE, params)
        .await
}

/// Append `text` to the agent's system context under `key`.
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
            with_member(
                connection,
                json!({"text": text, "source": "rkat-tui", "idempotency_key": key}),
            ),
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

/// The agent's live stream, opened again (after a conversation
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

/// The agent's conversations (`rkat/conversations`).
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn conversations(
    connection: &Connection,
) -> Result<Vec<crate::daemon::conversations::Listed>, ClientError> {
    let listed = call::<method::Conversations>(connection, &Empty {}).await?;
    Ok(listed.conversations)
}

/// `/new` (`session: None`) or `/resume` (`rkat/new`, `rkat/resume`): the
/// session the agent runs as afterwards, and a warning.
///
/// # Errors
///
/// The call's error (the daemon refused or failed the switch).
pub async fn switch(
    connection: &Connection,
    session: Option<&SessionId>,
) -> Result<(SessionId, Option<String>), ClientError> {
    let switched = match session {
        None => call::<method::New>(connection, &Empty {}).await?,
        Some(session) => call::<method::Resume>(connection, &session_params(session)).await?,
    };
    Ok((switched.session_id, switched.warning))
}

/// The parameters naming `session`.
fn session_params(session: &SessionId) -> SessionParams {
    SessionParams {
        session_id: session.clone(),
    }
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
    call::<method::Transcript>(connection, &session_params(session))
        .await
        .map(|transcript| transcript.messages)
}

/// A conversation's total token usage and its totals by model
/// (`rkat/usage`).
///
/// # Errors
///
/// The call's error, or [`ClientError::Decode`].
pub async fn usage(
    connection: &Connection,
    session: &SessionId,
) -> Result<crate::daemon::protocol::Usage, ClientError> {
    call::<method::Usage>(connection, &session_params(session)).await
}

/// The configured endpoint `session`'s auth binding names (`rkat/endpoint`;
/// `None`: a built-in endpoint).
///
/// # Errors
///
/// The call's error.
pub async fn session_endpoint(
    connection: &Connection,
    session: &SessionId,
) -> Result<Option<String>, ClientError> {
    call::<method::Endpoint>(connection, &session_params(session))
        .await
        .map(|answer| answer.endpoint)
}

/// `/compact now` for the agent: `mob/compact_member` (its next
/// compaction check compacts). Returns the engine's refusal as text.
///
/// # Errors
///
/// The call's error, as text.
pub async fn compact_member(connection: &Connection) -> Result<(), String> {
    connection
        .call(engine::COMPACT_MEMBER, member(connection))
        .await
        .map(drop)
        .map_err(|err| err.to_string())
}

/// `/compact now` through the agent's session (`session/compact`).
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
