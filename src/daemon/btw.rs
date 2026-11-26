//! `/btw`: a side question about the agent's session, answered by one
//! model call of its own that touches nothing of the agent's.
//!
//! The call runs in the daemon (`rkat/btw`, [`super::local`]): it has the
//! session's stored messages and builds model clients as the classifier's
//! are built (the engine factory, for the agent's current model on its
//! endpoint; a test's scripted client when one is installed). The request
//! carries no tools: [`SYSTEM`] as its system prompt, then one user
//! message with the session's record ([`record`], bounded by [`BUDGET`]),
//! the running tool's line, and the question ([`prompt`]). Nothing is
//! written to the agent's session: no message, no input, no run; its
//! activity is unchanged. Its tokens are counted under the session's usage
//! as a classifier call's are ([`super::usage::Ledger::side_question`]).

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use meerkat_core::{Message, SystemMessage, Usage, UserMessage};
use meerkat_llm_core::{LlmClient, LlmRequest};

/// The side call's system prompt.
pub const SYSTEM: &str = "You answer the user's side question about a coding session between \
them and their agent. Below is the session's record so far (the user's messages, the agent's \
replies, its tool calls and their results) and, while the agent works, its running turn as the \
user's screen shows it. Answer briefly, from what is there. You \
cannot act, run anything or call tools, and the agent does not see this question or your \
answer. When the answer is not in the record (for example the output of a command that has not \
finished), say so.";

/// The most bytes of the session's record a side call sends: past it, the
/// first user message and the most recent part are kept, and the record
/// says that the middle was left out.
pub const BUDGET: usize = 60_000;

/// The most bytes of one tool call's arguments or one result in the
/// record (the rest is cut, and says so).
const PART_MAX: usize = 4_000;

/// The answer's token bound.
const MAX_TOKENS: u32 = 1_024;

/// How long a side call may take.
pub const TIMEOUT: Duration = Duration::from_mins(1);

/// Builds the model client of a session's model identity (`None`: no
/// client can be built, with why).
pub type SideClients = Arc<
    dyn Fn(
            meerkat_core::SessionLlmIdentity,
        ) -> futures::future::BoxFuture<'static, Result<Arc<dyn LlmClient>, String>>
        + Send
        + Sync,
>;

/// The tool call the agent is running, as the client saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub name: String,
    /// How long it has run.
    pub secs: u64,
}

/// One message of the record, as a line block.
fn entry(
    message: &Message,
    names: &mut std::collections::HashMap<String, String>,
) -> Option<String> {
    match message {
        Message::User(user) => {
            let text = user.text_content();
            (!text.trim().is_empty()).then(|| format!("[user]\n{}", text.trim()))
        }
        Message::BlockAssistant(reply) => {
            let mut out = String::new();
            for block in &reply.blocks {
                if let meerkat_core::types::AssistantBlock::Text { text, .. } = block
                    && !text.trim().is_empty()
                {
                    let _ = writeln!(out, "[agent]\n{}", text.trim());
                }
            }
            for call in reply.tool_calls() {
                names.insert(call.id.to_string(), call.name.to_string());
                let _ = writeln!(
                    out,
                    "[agent calls {}] {}",
                    call.name,
                    cut(call.args.get(), PART_MAX)
                );
            }
            let out = out.trim_end().to_string();
            (!out.is_empty()).then_some(out)
        }
        Message::ToolResults { results, .. } => {
            let mut out = String::new();
            for result in results {
                let name = names
                    .get(&result.tool_use_id)
                    .map_or("a tool", String::as_str);
                let failed = if result.is_error { " (failed)" } else { "" };
                let _ = writeln!(
                    out,
                    "[result of {name}{failed}]\n{}",
                    cut(&result.text_content(), PART_MAX)
                );
            }
            let out = out.trim_end().to_string();
            (!out.is_empty()).then_some(out)
        }
        Message::System(_) | Message::SystemNotice(_) => None,
    }
}

/// `text`, cut to about `max` bytes (at a character boundary), saying so.
fn cut(text: &str, max: usize) -> String {
    match crate::text::head_bytes(text, max) {
        (_, false) => text.to_string(),
        (head, true) => format!("{head} … ({} more bytes left out)", text.len() - head.len()),
    }
}

/// The session's record for the side call, at most about `budget` bytes:
/// every user message, agent reply, tool call and result in order. Past
/// the budget the first user message is kept, then a line saying how many
/// messages in the middle were left out, then the most recent ones that
/// fit.
#[must_use]
pub fn record(messages: &[Message], budget: usize) -> String {
    let mut names = std::collections::HashMap::new();
    let entries: Vec<String> = messages
        .iter()
        .filter_map(|message| entry(message, &mut names))
        .collect();
    let whole: usize = entries.iter().map(|e| e.len() + 2).sum();
    if whole <= budget {
        return entries.join("\n\n");
    }
    let first = entries
        .iter()
        .position(|e| e.starts_with("[user]"))
        .unwrap_or(0);
    let head = cut(&entries[first], budget / 4);
    let mut room = budget.saturating_sub(head.len() + 100);
    let mut tail: Vec<&String> = Vec::new();
    for e in entries[first + 1..].iter().rev() {
        if e.len() + 2 > room {
            break;
        }
        room -= e.len() + 2;
        tail.push(e);
    }
    tail.reverse();
    let left_out = entries.len() - first - 1 - tail.len();
    let mut out = head;
    let _ = write!(
        out,
        "\n\n[… {left_out} messages in the middle of the session are left out here to keep this short …]"
    );
    for e in tail {
        out.push_str("\n\n");
        out.push_str(e);
    }
    out
}

/// `text`'s last `max` bytes or so (at a character boundary), saying what
/// was left out before them.
fn last_part(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let tail = crate::text::tail_bytes(text, max);
    format!(
        "(… its first {} bytes are left out here …)\n{tail}",
        text.len() - tail.len()
    )
}

/// The side call's user message: the record, the running turn as the
/// client shows it (the stored history has a run only once it ended; its
/// last [`BUDGET`]` / 2` bytes), the running tool's line, the question.
#[must_use]
pub fn prompt(
    record: &str,
    current_turn: Option<&str>,
    running: Option<&Running>,
    question: &str,
) -> String {
    let mut out = format!("<session record>\n{record}\n</session record>\n\n");
    if let Some(turn) = current_turn {
        let _ = write!(
            out,
            "<the running turn, as the user's screen shows it>\n{}\n</the running turn>\n\n",
            last_part(turn.trim(), BUDGET / 2)
        );
    }
    match running {
        Some(Running { name, secs }) => {
            let _ = writeln!(
                out,
                "Right now the agent is running the tool `{name}`; it has run for {secs} s and has not finished (its result is not in the record yet).\n"
            );
        }
        None => out.push_str("Right now the agent runs no tool.\n\n"),
    }
    let _ = write!(out, "The user's side question: {}", question.trim());
    out
}

/// Ask `model` on `client`: no tools, [`SYSTEM`] and `prompt`. The answer's
/// text and the usage the provider reported.
pub async fn ask(
    client: &dyn LlmClient,
    model: &str,
    prompt: String,
) -> Result<(String, Option<Usage>), String> {
    let request = LlmRequest {
        model: model.to_string(),
        messages: vec![
            Message::System(SystemMessage::new(SYSTEM.to_string())),
            Message::User(UserMessage::text(prompt)),
        ],
        tools: Vec::new(),
        max_tokens: MAX_TOKENS,
        temperature: None,
        stop_sequences: None,
        provider_params: None,
        tool_choice: meerkat_core::ToolChoice::default(),
    };
    crate::approval::classifier::text_call(client, request)
        .await
        .map_err(|err| format!("the model call failed: {err}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message::User(UserMessage::text(text.to_string()))
    }

    /// Within the budget every message is in the record, in order; past
    /// it the first user message and the most recent part are kept, and
    /// the middle is said to be left out.
    #[test]
    fn a_long_session_keeps_its_first_message_and_its_latest_part() {
        let short = vec![user("first question"), user("second")];
        let text = record(&short, BUDGET);
        assert!(text.contains("first question") && text.contains("second"));
        assert!(!text.contains("left out"));

        let mut long = vec![user("THE FIRST MESSAGE")];
        for n in 0..400 {
            long.push(user(&format!("message {n} {}", "x".repeat(400))));
        }
        let text = record(&long, BUDGET);
        assert!(text.len() <= BUDGET, "{}", text.len());
        assert!(text.starts_with("[user]\nTHE FIRST MESSAGE"));
        assert!(text.contains("messages in the middle of the session are left out"));
        assert!(text.contains("message 399 "));
        assert!(!text.contains("message 0 "));
        // The latest part is whole and in order.
        let at = |n: usize| text.find(&format!("message {n} ")).unwrap();
        assert!(at(398) < at(399));
    }

    #[test]
    fn the_prompt_names_the_running_tool_and_the_question() {
        let running = Running {
            name: "shell".into(),
            secs: 95,
        };
        let text = prompt(
            "[user]\nhi",
            Some("## user\nbuild it"),
            Some(&running),
            " what runs? ",
        );
        assert!(text.contains("running the tool `shell`; it has run for 95 s"));
        assert!(
            text.contains("<the running turn, as the user's screen shows it>\n## user\nbuild it")
        );
        assert!(text.ends_with("The user's side question: what runs?"));
        let idle = prompt("r", None, None, "q");
        assert!(idle.contains("runs no tool"));
        assert!(!idle.contains("running turn"));
        // A long running turn keeps its end.
        let long = format!("START {} END", "y".repeat(BUDGET));
        let text = prompt("r", Some(&long), None, "q");
        assert!(text.contains("END") && !text.contains("START"));
        assert!(text.contains("bytes are left out here"));
    }

    #[test]
    fn a_long_part_is_cut_and_says_so() {
        let text = cut(&"é".repeat(5_000), 101);
        assert!(text.contains("more bytes left out"));
    }
}
