//! The transcript of a stored conversation, as blocks.

use meerkat_core::{AssistantBlock, Message, TranscriptUserRole};

use super::{Block, ToolState};

/// Transcript blocks for a stored conversation's `messages` (read from the
/// daemon): user text, assistant text and tool calls. Tool results are
/// summarized as completed calls; runtime notices, injected context and
/// compaction summaries are not shown.
#[must_use]
pub fn blocks_from_messages(messages: &[Message]) -> Vec<Block> {
    let mut blocks = Vec::new();
    // The reply to `/compact`'s own prompt is not shown either.
    let mut compact_reply = false;
    for message in messages {
        match message {
            Message::User(user) if user.transcript_role == TranscriptUserRole::Conversational => {
                let text = user.text_content();
                compact_reply = text == crate::ui::COMPACT_PROMPT;
                if !text.is_empty() && !compact_reply {
                    blocks.push(Block::User { text });
                }
            }
            Message::BlockAssistant(_) if compact_reply => compact_reply = false,
            Message::BlockAssistant(assistant) => {
                for block in &assistant.blocks {
                    if let AssistantBlock::Text { text, .. } = block
                        && !text.is_empty()
                    {
                        blocks.push(Block::Assistant { md: text.clone() });
                    }
                }
                for call in assistant.tool_calls() {
                    let args = serde_json::from_str(call.args.get())
                        .unwrap_or_else(|_| serde_json::Value::String(call.args.get().to_string()));
                    blocks.push(restored_tool_block(call.name, args));
                }
            }
            Message::SystemNotice(notice) => {
                blocks.extend(peer_blocks(notice));
                blocks.extend(outcome_blocks(notice));
            }
            _ => {}
        }
    }
    blocks
}

/// The peer messages another agent sent, of a committed system notice
/// (a comms notice's incoming messages).
fn peer_blocks(notice: &meerkat_core::SystemNoticeMessage) -> Vec<Block> {
    use meerkat_core::{
        CommsNoticeKind, SystemNoticeBlock, SystemNoticeDirection, SystemNoticeKind,
    };
    if notice.kind != SystemNoticeKind::Comms {
        return Vec::new();
    }
    notice
        .blocks
        .iter()
        .filter_map(|block| match block {
            SystemNoticeBlock::Comms {
                kind: CommsNoticeKind::Message,
                direction: SystemNoticeDirection::Incoming,
                peer,
                content,
                ..
            } => {
                let from = peer
                    .as_ref()
                    .and_then(|peer| peer.display_name.clone())
                    .unwrap_or_default();
                let text: String = content
                    .iter()
                    .filter_map(|block| match block {
                        meerkat_core::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let body = crate::ui::peer::body(&from, &text);
                Some(Block::Peer {
                    from: crate::ui::peer::short_name(&from).to_string(),
                    text: body,
                })
            }
            _ => None,
        })
        .collect()
}

/// A detached job's durable outcome record (a fork's report) as the live
/// transcript shows it ([`super::outcome::JobOutcome`]); a detail of
/// another shape is left out, as before.
fn outcome_blocks(notice: &meerkat_core::SystemNoticeMessage) -> Vec<Block> {
    use meerkat_core::SystemNoticeBlock;
    notice
        .blocks
        .iter()
        .filter_map(|block| match block {
            SystemNoticeBlock::BackgroundJob {
                job_id,
                display_name,
                detail: Some(detail),
                ..
            } => super::outcome::JobOutcome::from_detail(
                display_name.as_deref().unwrap_or("job"),
                Some(job_id),
                detail,
            )
            .map(|outcome| outcome.block()),
            _ => None,
        })
        .collect()
}

/// A stored call as a done tool block (its line's paths are made again
/// with the transcript's path view, [`super::Transcript::reset`]).
fn restored_tool_block(name: &str, args: serde_json::Value) -> Block {
    let head = super::tool_line::head(name, &args, &super::tool_line::PathView::default());
    Block::Tool {
        name: name.to_string(),
        args,
        head,
        outcome: super::tool_line::Outcome::default(),
        ticked: 0,
        state: ToolState::Done {
            is_error: false,
            duration_ms: 0,
        },
        result_preview: None,
        diff: None,
        note: None,
        depth: 0,
        collapsed: true,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use meerkat_core::UserMessage;

    /// A committed peer message (a comms notice, as the engine stores an
    /// incoming one) is restored as a block labelled with its sender; an
    /// outgoing one is not shown.
    #[test]
    fn a_committed_peer_message_is_restored_labelled() {
        use meerkat_core::{
            CommsNoticeKind, ContentBlock, SystemNoticeBlock, SystemNoticeDirection,
            SystemNoticeKind, SystemNoticeMessage, SystemNoticePeer,
        };
        let comms = |direction| SystemNoticeBlock::Comms {
            kind: CommsNoticeKind::Message,
            direction,
            peer: Some(SystemNoticePeer {
                id: serde_json::from_value(serde_json::json!(
                    "46d5fbeb-acb0-5a56-a10f-31dd0fb07f9e"
                ))
                .unwrap(),
                display_name: Some("tui/main/main-gak7".into()),
            }),
            sender_taint: None,
            request_id: None,
            intent: None,
            status: None,
            summary: Some("Peer message".into()),
            payload: None,
            content: vec![ContentBlock::Text {
                text: "Peer message from tui/main/main-gak7:\nI am changing src/lib.rs".into(),
            }],
            delivery_id: None,
            in_reply_to: None,
        };
        let notice = |direction| {
            Message::SystemNotice(SystemNoticeMessage {
                kind: SystemNoticeKind::Comms,
                runtime_origin: None,
                body: Some("Peer message".into()),
                blocks: vec![comms(direction)],
                created_at: meerkat_core::types::message_timestamp_now(),
            })
        };
        assert_eq!(
            blocks_from_messages(&[
                notice(SystemNoticeDirection::Incoming),
                notice(SystemNoticeDirection::Outgoing)
            ]),
            vec![Block::Peer {
                from: "main-gak7".into(),
                text: "I am changing src/lib.rs".into()
            }]
        );
    }

    /// A fork's durable outcome record restores as the compact block the
    /// live transcript shows.
    #[test]
    fn a_fork_outcome_restores_as_its_compact_block() {
        let detail = serde_json::json!({
            "agent_identity": "kid",
            "bounded_result": {"label": "fork_off_result", "status": "completed", "text": "done well"},
            "member_ref": "x",
            "status": "completed",
            "turns": 1,
            "tool_calls": 0
        })
        .to_string();
        let notice = meerkat_core::SystemNoticeMessage::persisted_background_job(
            "fork_off",
            "job-9",
            meerkat_core::event::BackgroundJobTerminalStatus::Completed,
            detail.clone(),
        );
        let restored = blocks_from_messages(&[Message::SystemNotice(notice)]);
        let live =
            super::super::outcome::JobOutcome::from_detail("fork_off", Some("job-9"), &detail)
                .unwrap()
                .block();
        assert_eq!(restored, vec![live]);
    }

    #[test]
    fn restored_blocks_skip_injected_context() {
        let messages = vec![
            Message::User(UserMessage::text("hello")),
            Message::User(UserMessage::injected_context("ambient")),
        ];
        assert_eq!(
            blocks_from_messages(&messages),
            vec![Block::User {
                text: "hello".into()
            }]
        );
    }
}
