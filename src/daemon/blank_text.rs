//! No empty or whitespace-only text on the wire.
//!
//! Some models (seen behind a LiteLLM proxy) send a line break, or an empty
//! text part, before each tool call. The engine stores it as the step's
//! text and its dialect clients send it back on every later request: on
//! Chat Completions as the assistant message's `content: "\n"`, on the
//! Responses API as a `message` item whose `output_text` is `"\n"` or `""`
//! (an item that carries the id the server gave it is replayed even when
//! its text is empty). A proxy that forwards to a provider refusing empty
//! content replaces it with a placeholder sentence (LiteLLM's `[System:
//! Empty message content sanitised to satisfy protocol]`), which the model
//! then reads as its own earlier words. Anthropic refuses a text block that
//! is only whitespace outright.
//!
//! [`for_the_wire`] is applied by the daemon's client decorator
//! (`super::schema_compat::SchemaCompat`) to every request's messages, so
//! what is stored stays as the model sent it and only what is sent
//! changes:
//!
//! - an assistant `Text` or `Transcript` block that is empty after
//!   trimming is left out, unless it carries a provider signature the
//!   provider needs back (Gemini's thought signature, Anthropic's
//!   metadata, an OpenAI reasoning item's): only blocks with no metadata
//!   or the OpenAI response / message identity (`OpenAiResponse`,
//!   `OpenAiAssistantMessage`) go. Without them the step lowers as the
//!   engine lowers a step it does not replay as received: its text and
//!   tool calls in plain form. Reasoning, tool calls and every other block
//!   stay as they are.
//! - an assistant message left with no block at all is left out (it would
//!   lower to an assistant message with no content and no tool call).
//! - a tool result whose content is only text and empty after trimming is
//!   sent as [`NO_OUTPUT`], paired with its call as before (the other
//!   classic trigger of the placeholder; our own tools never return one).
//!
//! User messages, system messages and notices are never changed.

use meerkat_core::{AssistantBlock, ContentBlock, Message, ProviderMeta, ToolResult};

/// What a tool result with no text is sent as.
pub const NO_OUTPUT: &str = "(no output)";

/// `messages` as they go on the wire (see the module docs); `None` when
/// that is `messages` unchanged, so the request is the engine's own.
#[must_use]
pub fn for_the_wire(messages: &[Message]) -> Option<Vec<Message>> {
    if !messages.iter().any(changes) {
        return None;
    }
    Some(
        messages
            .iter()
            .filter_map(|message| match message {
                Message::BlockAssistant(assistant) if assistant.blocks.iter().any(left_out) => {
                    let mut assistant = assistant.clone();
                    assistant.blocks.retain(|block| !left_out(block));
                    (!assistant.blocks.is_empty()).then_some(Message::BlockAssistant(assistant))
                }
                Message::BlockAssistant(assistant) if assistant.blocks.is_empty() => None,
                Message::ToolResults {
                    results,
                    created_at,
                } if results.iter().any(blank_result) => Some(Message::ToolResults {
                    results: results
                        .iter()
                        .map(|result| {
                            let mut result = result.clone();
                            if blank_result(&result) {
                                result.content = vec![ContentBlock::Text {
                                    text: NO_OUTPUT.to_string(),
                                }];
                            }
                            result
                        })
                        .collect(),
                    created_at: *created_at,
                }),
                other => Some(other.clone()),
            })
            .collect(),
    )
}

/// Whether [`for_the_wire`] changes `message`.
fn changes(message: &Message) -> bool {
    match message {
        Message::BlockAssistant(assistant) => {
            assistant.blocks.is_empty() || assistant.blocks.iter().any(left_out)
        }
        Message::ToolResults { results, .. } => results.iter().any(blank_result),
        _ => false,
    }
}

/// Whether `block` is text with nothing in it that a provider needs back.
fn left_out(block: &AssistantBlock) -> bool {
    match block {
        AssistantBlock::Text { text, meta } | AssistantBlock::Transcript { text, meta, .. } => {
            text.trim().is_empty()
                && matches!(
                    meta.as_deref(),
                    None | Some(
                        ProviderMeta::OpenAiResponse { .. }
                            | ProviderMeta::OpenAiAssistantMessage { .. }
                    )
                )
        }
        _ => false,
    }
}

/// Whether `result` is text only, and empty after trimming.
fn blank_result(result: &ToolResult) -> bool {
    result
        .content
        .iter()
        .all(|block| matches!(block, ContentBlock::Text { text } if text.trim().is_empty()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use meerkat_core::{BlockAssistantMessage, StopReason, UserMessage};

    fn text(text: &str, meta: Option<ProviderMeta>) -> AssistantBlock {
        AssistantBlock::Text {
            text: text.into(),
            meta: meta.map(Box::new),
        }
    }

    fn call(id: &str) -> AssistantBlock {
        AssistantBlock::ToolUse {
            id: id.into(),
            name: "read_file".into(),
            args: serde_json::value::RawValue::from_string(r#"{"path":"a"}"#.into()).unwrap(),
            meta: None,
        }
    }

    fn assistant(blocks: Vec<AssistantBlock>) -> Message {
        Message::BlockAssistant(BlockAssistantMessage::new(blocks, StopReason::ToolUse))
    }

    fn results(content: &[&str]) -> Message {
        Message::tool_results(
            content
                .iter()
                .enumerate()
                .map(|(n, text)| ToolResult::new(format!("c{n}"), (*text).to_string(), false))
                .collect(),
        )
    }

    fn blocks(message: &Message) -> &[AssistantBlock] {
        match message {
            Message::BlockAssistant(a) => &a.blocks,
            other => panic!("not an assistant message: {other:?}"),
        }
    }

    fn message_meta() -> ProviderMeta {
        ProviderMeta::OpenAiAssistantMessage {
            id: "msg_1".into(),
            phase: None,
            response_id: Some("resp_1".into()),
        }
    }

    /// Empty and whitespace-only text before a tool call goes, with no
    /// metadata, the OpenAI response id or the OpenAI message identity; the
    /// tool call stays as it was.
    #[test]
    fn blank_text_before_a_tool_call_is_left_out() {
        for meta in [
            None,
            Some(ProviderMeta::OpenAiResponse {
                response_id: "resp_1".into(),
            }),
            Some(message_meta()),
        ] {
            for blank in ["", "\n", " \n\t "] {
                let messages = vec![
                    Message::User(UserMessage::text("go")),
                    assistant(vec![text(blank, meta.clone()), call("c0")]),
                ];
                let sent = for_the_wire(&messages).expect("changed");
                assert_eq!(sent.len(), 2);
                let kept = blocks(&sent[1]);
                assert_eq!(kept.len(), 1, "{blank:?} {meta:?}");
                assert!(
                    matches!(&kept[0], AssistantBlock::ToolUse { id, args, .. } if id == "c0" && args.get() == r#"{"path":"a"}"#)
                );
            }
        }
    }

    /// A conversation with nothing blank is sent as it is (no copy).
    #[test]
    fn a_conversation_without_blank_text_is_unchanged() {
        let messages = vec![
            Message::User(UserMessage::text("go")),
            assistant(vec![text("\nLet me look.", None), call("c0")]),
            results(&["found"]),
            assistant(vec![text("done", Some(message_meta()))]),
        ];
        assert!(for_the_wire(&messages).is_none());
    }

    /// Text that carries a provider signature stays, blank or not:
    /// Gemini's thought signature, Anthropic's metadata, an OpenAI
    /// reasoning item's; and so do reasoning blocks, signed or not.
    #[test]
    fn signed_blocks_and_reasoning_are_untouched() {
        let signed = [
            ProviderMeta::Gemini {
                thought_signature: "sig".into(),
            },
            ProviderMeta::Anthropic {
                signature: "sig".into(),
            },
        ];
        for meta in signed {
            let messages = vec![assistant(vec![text("\n", Some(meta.clone())), call("c0")])];
            assert!(for_the_wire(&messages).is_none(), "{meta:?}");
        }
        let thinking = AssistantBlock::Reasoning {
            text: String::new(),
            meta: Some(Box::new(ProviderMeta::Anthropic {
                signature: "sig-abc".into(),
            })),
        };
        let messages = vec![assistant(vec![thinking, text("\n", None), call("c0")])];
        let sent = for_the_wire(&messages).unwrap();
        let kept = blocks(&sent[0]);
        assert_eq!(kept.len(), 2);
        assert!(matches!(
            &kept[0],
            AssistantBlock::Reasoning { text, meta: Some(meta) }
                if text.is_empty() && matches!(meta.as_ref(), ProviderMeta::Anthropic { signature } if signature == "sig-abc")
        ));
        assert!(matches!(&kept[1], AssistantBlock::ToolUse { .. }));
    }

    /// An assistant message that is left with nothing (or had nothing) is
    /// not sent; the messages around it are.
    #[test]
    fn a_message_left_empty_is_left_out() {
        for message in [assistant(vec![text("\n", None)]), assistant(Vec::new())] {
            let messages = vec![
                Message::User(UserMessage::text("one")),
                message,
                Message::User(UserMessage::text("two")),
            ];
            let sent = for_the_wire(&messages).unwrap();
            assert_eq!(sent.len(), 2);
            assert!(sent.iter().all(|m| matches!(m, Message::User(_))));
        }
    }

    /// A tool result with no text is sent as "(no output)", still paired
    /// with its call; the other results of the step are as they were.
    #[test]
    fn a_blank_tool_result_says_no_output() {
        let messages = vec![
            assistant(vec![call("c0"), call("c1"), call("c2")]),
            results(&["", "found", " \n"]),
        ];
        let sent = for_the_wire(&messages).unwrap();
        let Message::ToolResults { results, .. } = &sent[1] else {
            panic!("{sent:?}");
        };
        let shown: Vec<(&str, String)> = results
            .iter()
            .map(|r| (r.tool_use_id.as_str(), r.text_content()))
            .collect();
        assert_eq!(
            shown,
            [
                ("c0", NO_OUTPUT.to_string()),
                ("c1", "found".to_string()),
                ("c2", NO_OUTPUT.to_string())
            ]
        );
        assert_eq!(blocks(&sent[0]).len(), 3);
    }

    /// User messages are never changed, empty or not.
    #[test]
    fn user_messages_are_never_changed() {
        let messages = vec![Message::User(UserMessage::text(" "))];
        assert!(for_the_wire(&messages).is_none());
    }
}
