//! Tool-call ids another provider refuses, made short on the wire.
//!
//! A conversation keeps the tool-call ids of the model that issued them,
//! and the engine's dialect clients send them back unchanged on every
//! later request (`meerkat-openai` writes `call_id` from the assistant's
//! tool use and from the tool result, `client_compatible` writes `id` and
//! `tool_call_id`; `meerkat-anthropic` writes `id` and `tool_use_id`). A
//! model behind a proxy may issue ids thousands of characters long (one
//! that packs a thought signature into the id), and after a `/model`
//! switch to an OpenAI-dialect model every request is refused with a 400
//! saying a `call_id` is too long ("Expected a string with maximum length
//! 64"); `/compact` fails the same way, its request carrying the same
//! history.
//!
//! [`for_the_wire`] is applied by the daemon's client decorator
//! (`super::schema_compat::SchemaCompat`) to every request's messages, so
//! what is stored stays as the model sent it (the issuing model gets its
//! own ids back after a switch back) and only what is sent changes: on a
//! request to a dialect that would refuse an id, that id becomes
//! [`short_id`] of it in both the assistant's tool call and the matching
//! tool result. The id is a digest of the original, so the same id gets
//! the same short one across the whole request and across requests, and
//! two different ids stay different. An id the target accepts is left
//! exactly as it is: nothing changes for a conversation that never
//! switched. Upstream: lukacf/meerkat#2560; this filter goes when it
//! ships.

use meerkat_core::{AssistantBlock, Message, Provider};

/// The longest tool-call id the OpenAI APIs take (`call_id`, `tool_call_id`:
/// "Expected a string with maximum length 64"); self-hosted endpoints speak
/// that dialect and are held to it too.
pub const OPENAI_MAX_ID: usize = 64;

/// The longest tool-use id sent to Anthropic: its ids are documented as
/// `[a-zA-Z0-9_-]`; a length bound of ours, as OpenAI's (a short id is
/// harmless where a longer one would be taken).
pub const ANTHROPIC_MAX_ID: usize = 64;

/// The digits of the digest a short id keeps.
const SHORT_DIGITS: usize = 32;

/// Whether `provider`'s dialect takes `id` as a tool-call id.
#[must_use]
pub fn accepted(provider: Provider, id: &str) -> bool {
    match provider {
        Provider::OpenAI | Provider::SelfHosted => !id.is_empty() && id.len() <= OPENAI_MAX_ID,
        Provider::Anthropic => {
            !id.is_empty()
                && id.len() <= ANTHROPIC_MAX_ID
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        }
        // Gemini matches calls by its own ids or by name; nothing is known
        // it refuses.
        Provider::Gemini | Provider::Other => true,
    }
}

/// The short, stable id `id` is sent as: `call_` and the first
/// [`SHORT_DIGITS`] hex digits of its SHA-256.
#[must_use]
pub fn short_id(id: &str) -> String {
    let mut digest = crate::hex::sha256(id.as_bytes());
    digest.truncate(SHORT_DIGITS);
    format!("call_{digest}")
}

/// `messages` as they go to `provider` (see the module docs); `None` when
/// that is `messages` unchanged.
#[must_use]
pub fn for_the_wire(provider: Provider, messages: &[Message]) -> Option<Vec<Message>> {
    let refused = |id: &str| !accepted(provider, id);
    let changes = messages.iter().any(|message| match message {
        Message::BlockAssistant(assistant) => assistant
            .blocks
            .iter()
            .any(|block| matches!(block, AssistantBlock::ToolUse { id, .. } if refused(id))),
        Message::ToolResults { results, .. } => {
            results.iter().any(|result| refused(&result.tool_use_id))
        }
        _ => false,
    });
    if !changes {
        return None;
    }
    Some(
        messages
            .iter()
            .map(|message| match message {
                Message::BlockAssistant(assistant) => {
                    let mut assistant = assistant.clone();
                    for block in &mut assistant.blocks {
                        if let AssistantBlock::ToolUse { id, .. } = block
                            && refused(id)
                        {
                            *id = short_id(id);
                        }
                    }
                    Message::BlockAssistant(assistant)
                }
                Message::ToolResults {
                    results,
                    created_at,
                } => {
                    let mut results = results.clone();
                    for result in &mut results {
                        if refused(&result.tool_use_id) {
                            result.tool_use_id = short_id(&result.tool_use_id);
                        }
                    }
                    Message::ToolResults {
                        results,
                        created_at: *created_at,
                    }
                }
                other => other.clone(),
            })
            .collect(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use meerkat_core::{BlockAssistantMessage, StopReason, ToolResult};

    fn history(ids: &[&str]) -> Vec<Message> {
        let calls = ids
            .iter()
            .map(|id| AssistantBlock::ToolUse {
                id: (*id).to_string(),
                name: "shell".into(),
                args: serde_json::value::to_raw_value(&serde_json::json!({"command": "ls"}))
                    .unwrap(),
                meta: None,
            })
            .collect();
        vec![
            Message::BlockAssistant(BlockAssistantMessage::new(calls, StopReason::ToolUse)),
            Message::tool_results(
                ids.iter()
                    .map(|id| ToolResult::new((*id).to_string(), "ok".into(), false))
                    .collect(),
            ),
        ]
    }

    fn ids(messages: &[Message]) -> (Vec<String>, Vec<String>) {
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for message in messages {
            match message {
                Message::BlockAssistant(assistant) => {
                    for block in &assistant.blocks {
                        if let AssistantBlock::ToolUse { id, .. } = block {
                            calls.push(id.clone());
                        }
                    }
                }
                Message::ToolResults { results: r, .. } => {
                    results.extend(r.iter().map(|r| r.tool_use_id.clone()));
                }
                _ => {}
            }
        }
        (calls, results)
    }

    /// A 7000-character id on an OpenAI-dialect request is sent as one
    /// short id, on the call and on its result; two long ids stay apart;
    /// a short id is untouched; the same history gives the same ids.
    #[test]
    fn a_long_id_is_short_on_the_wire_and_paired_with_its_result() {
        let long_a = "a".repeat(7000);
        let long_b = format!("{}b", "a".repeat(6999));
        let history = history(&[&long_a, &long_b, "call_1"]);
        for provider in [Provider::OpenAI, Provider::SelfHosted, Provider::Anthropic] {
            let sent = for_the_wire(provider, &history).unwrap();
            let (calls, results) = ids(&sent);
            assert_eq!(calls, results, "{provider:?}");
            assert_eq!(calls[0], short_id(&long_a));
            assert!(calls[0].len() <= OPENAI_MAX_ID && accepted(provider, &calls[0]));
            assert_ne!(calls[0], calls[1]);
            assert_eq!(calls[2], "call_1");
            let again = for_the_wire(provider, &history).unwrap();
            assert_eq!(ids(&again), (calls.clone(), results));
        }
        // The stored history is not changed.
        assert_eq!(ids(&history).0[0], long_a);
    }

    /// A request whose ids its dialect takes is the engine's own: the one
    /// that issued them (Gemini) and short ids anywhere.
    #[test]
    fn ids_the_target_takes_are_left_alone() {
        let long = "x".repeat(7000);
        assert!(for_the_wire(Provider::Gemini, &history(&[&long])).is_none());
        assert!(for_the_wire(Provider::OpenAI, &history(&["call_1", "toolu_01"])).is_none());
        assert!(for_the_wire(Provider::Anthropic, &history(&["toolu_01"])).is_none());
        // Anthropic takes only `[a-zA-Z0-9_-]`.
        assert!(for_the_wire(Provider::Anthropic, &history(&["call:1"])).is_some());
    }
}
