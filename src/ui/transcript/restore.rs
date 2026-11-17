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
                    // A note of helpers' endings the daemon started a run
                    // with is folded as the live view shows it.
                    blocks.push(restored_note(&text).unwrap_or(Block::User { text }));
                }
            }
            Message::BlockAssistant(_) if compact_reply => compact_reply = false,
            Message::BlockAssistant(assistant) => {
                blocks.extend(prose_block(&assistant.blocks));
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

/// A stored user message that is a whole note of helpers' endings (the
/// daemon's words, [`crate::daemon::agents::endings::note_summary`]), as
/// its folded block.
fn restored_note(text: &str) -> Option<Block> {
    crate::daemon::agents::endings::note_summary(text)?;
    super::helper_note(crate::daemon::agents::HELPER_ENDED_LABEL, text, true)
}

/// The prose of one stored assistant message (one model step), by the
/// module's rule ([`super`]): its text blocks joined as they are, as the
/// engine joins them, whatever provider parts or items they were; a
/// restored conversation shows no reasoning, so nothing visible parts
/// them. Text that is only whitespace shows nothing and is no block.
fn prose_block(blocks: &[AssistantBlock]) -> Option<Block> {
    let md: String = blocks
        .iter()
        .filter_map(|block| match block {
            AssistantBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (!md.trim().is_empty()).then_some(Block::Assistant { md })
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
                    collapsed: true,
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
                text: "I am changing src/lib.rs".into(),
                collapsed: true,
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

    /// The stored shape of an answer a provider cut into two message items
    /// (each text block with its own item's meta) with a blank reasoning
    /// and a blank text between, before a tool call: one prose block, the
    /// parts joined as they are, then the call; a message of blank text
    /// only is no block.
    #[test]
    fn a_stored_answer_cut_into_parts_restores_as_one_block() {
        use meerkat_core::{BlockAssistantMessage, ProviderMeta, StopReason};
        let item = |id: &str| {
            Some(Box::new(ProviderMeta::OpenAi {
                id: id.into(),
                encrypted_content: None,
                phase: None,
                response_id: Some("resp_1".into()),
            }))
        };
        let text = |text: &str, id: &str| AssistantBlock::Text {
            text: text.into(),
            meta: item(id),
        };
        let answer = BlockAssistantMessage::new(
            vec![
                text("> The old reader was remov", "msg_1"),
                AssistantBlock::Reasoning {
                    text: " \n".into(),
                    meta: None,
                },
                text("", "msg_2"),
                text("ed; the new one stays.", "msg_3"),
                AssistantBlock::ToolUse {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    args: serde_json::value::RawValue::from_string(r#"{"path":"a"}"#.into())
                        .unwrap(),
                    meta: None,
                },
            ],
            StopReason::ToolUse,
        );
        let blank = BlockAssistantMessage::new(vec![text(" \n", "msg_4")], StopReason::EndTurn);
        let restored = blocks_from_messages(&[
            Message::BlockAssistant(answer),
            Message::BlockAssistant(blank),
        ]);
        assert_eq!(restored.len(), 2, "{restored:#?}");
        assert_eq!(
            restored[0],
            Block::Assistant {
                md: "> The old reader was removed; the new one stays.".into()
            }
        );
        assert!(matches!(&restored[1], Block::Tool { name, .. } if name == "read_file"));
    }

    /// A conversation re-opened shows a note of a helper's ending the daemon
    /// started a run with as the live view does, folded, not as a prompt
    /// of the user's; a note that travelled with the user's words stays
    /// the user's message.
    #[test]
    fn a_stored_helper_ending_note_restores_folded() {
        use crate::daemon::agents::endings::{NoteItem, note_text};
        let note = note_text(&[NoteItem {
            label: "review-x".into(),
            how: crate::mobs::endings::How::Done {
                answer: "All good.\nDetails follow.".into(),
            },
            rest: None,
        }]);
        let held = format!(
            "{note}\n\n{}\n\nnow fix it",
            crate::daemon::local::HELD_NOTE_SEPARATOR
        );
        let restored = blocks_from_messages(&[
            Message::User(UserMessage::text(note.clone())),
            Message::User(UserMessage::text(held.clone())),
        ]);
        assert_eq!(
            restored,
            vec![
                Block::HelperNote {
                    title: "helper review-x finished".into(),
                    first: "All good.".into(),
                    text: note,
                    helpers: vec!["review-x".into()],
                    failed: false,
                    collapsed: true,
                    paired: false,
                },
                Block::User { text: held },
            ]
        );
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
