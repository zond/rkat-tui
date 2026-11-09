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
    for message in messages {
        match message {
            Message::User(user) if user.transcript_role == TranscriptUserRole::Conversational => {
                let text = user.text_content();
                if !text.is_empty() {
                    blocks.push(Block::User { text });
                }
            }
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
            _ => {}
        }
    }
    blocks
}

fn restored_tool_block(name: &str, args: serde_json::Value) -> Block {
    Block::Tool {
        name: name.to_string(),
        args,
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
mod tests {
    use super::*;
    use meerkat_core::UserMessage;

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
