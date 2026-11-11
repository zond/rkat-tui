//! What the daemon pushes to a client, decoded once: each notification
//! method the runner acts on is one [`Push`] variant with its typed
//! parameters, as each request is one [`crate::daemon::protocol::Method`].

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::remote::{STREAM_END, STREAM_EVENT};
use super::transport::Notification;
use crate::daemon::protocol;

/// One notification from the daemon.
#[derive(Debug, Clone)]
pub enum Push {
    /// The engine's notice of a durable decision (an approval dialog).
    Decision(Box<meerkat_contracts::WireDecisionNotice>),
    /// A decision ended ([`protocol::DECISION_ENDED`]).
    DecisionEnded(protocol::DecisionEnded),
    /// What the approval engine reported ([`protocol::APPROVAL_NOTE`]).
    Approval(protocol::ApprovalNote),
    /// A file tool's change ([`protocol::FILE_DIFF`]).
    FileDiff(protocol::FileDiffNote),
    /// A schedule or a watch started a turn ([`protocol::TRIGGERED`]).
    Triggered(protocol::TriggeredNote),
    /// A conversation's totals ([`protocol::COST`]).
    Cost(protocol::CostNote),
    /// An agent moved to another conversation ([`protocol::SWITCHED`]).
    Switched(protocol::SwitchedNote),
    /// A mob hosts helpers now, or was destroyed ([`protocol::MOB`]).
    Mob(protocol::MobNote),
    /// The permission mode and the denial counters ([`protocol::MODE`]).
    Mode(crate::approval::desk::ModeStatus),
    /// An agent is idle ([`protocol::IDLE`]).
    Idle(protocol::IdleNote),
    /// An event of the stream `stream` (the conversation's own, or a mob's;
    /// which one decides how `event` reads).
    Stream {
        /// The stream.
        stream: String,
        /// The event, as sent.
        event: Value,
        /// The stream's sequence space, when it names one.
        epoch: Option<String>,
    },
    /// The stream `0` ended.
    StreamEnd(String),
}

/// The parameters of a stream's event (`mob/stream_event`).
#[derive(Deserialize)]
struct StreamEvent {
    stream_id: String,
    event: Value,
    #[serde(default)]
    epoch: Option<String>,
}

/// The parameters of a stream's end (`mob/stream_end`).
#[derive(Deserialize)]
struct StreamEnded {
    stream_id: String,
}

impl Push {
    /// The push `note` carries; `None` for a method the client does not act
    /// on, or parameters it cannot read (logged).
    #[must_use]
    pub fn decode(note: Notification) -> Option<Self> {
        let push = match note.method.as_str() {
            protocol::DECISION_NOTICE => Self::Decision(Box::new(read(note)?)),
            protocol::DECISION_ENDED => Self::DecisionEnded(read(note)?),
            protocol::APPROVAL_NOTE => Self::Approval(read(note)?),
            protocol::FILE_DIFF => Self::FileDiff(read(note)?),
            protocol::TRIGGERED => Self::Triggered(read(note)?),
            protocol::COST => Self::Cost(read(note)?),
            protocol::SWITCHED => Self::Switched(read(note)?),
            protocol::MOB => Self::Mob(read(note)?),
            protocol::IDLE => Self::Idle(read(note)?),
            protocol::MODE => Self::Mode(read(note)?),
            STREAM_EVENT => {
                let StreamEvent {
                    stream_id,
                    event,
                    epoch,
                } = read(note)?;
                Self::Stream {
                    stream: stream_id,
                    event,
                    epoch,
                }
            }
            STREAM_END => Self::StreamEnd(read::<StreamEnded>(note)?.stream_id),
            _ => return None,
        };
        Some(push)
    }
}

/// `note`'s parameters as `T`; `None` (logged) when they do not read.
fn read<T: DeserializeOwned>(note: Notification) -> Option<T> {
    let Notification { method, params } = note;
    serde_json::from_value(params)
        .map_err(|err| tracing::debug!(error = %err, %method, "unreadable notification"))
        .ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn note(method: &str, params: Value) -> Notification {
        Notification {
            method: method.to_string(),
            params,
        }
    }

    #[test]
    fn each_method_decodes_to_its_push_and_nothing_else_does() {
        let ended = Push::decode(note(
            protocol::DECISION_ENDED,
            json!({"decision_id": "d1", "phase": "rejected"}),
        ));
        assert!(matches!(ended, Some(Push::DecisionEnded(e)) if e.decision_id == "d1"));
        let event = Push::decode(note(
            STREAM_EVENT,
            json!({"stream_id": "s1", "event": {"x": 1}, "epoch": "e1"}),
        ));
        assert!(
            matches!(event, Some(Push::Stream { stream, epoch: Some(epoch), .. }) if stream == "s1" && epoch == "e1")
        );
        let end = Push::decode(note(STREAM_END, json!({"stream_id": "s1"})));
        assert!(matches!(end, Some(Push::StreamEnd(stream)) if stream == "s1"));
        // Unreadable parameters and unknown methods are not acted on.
        assert!(Push::decode(note(protocol::COST, json!({"nope": true}))).is_none());
        assert!(Push::decode(note("rkat/unknown", json!({}))).is_none());
    }
}
