//! Messages another agent of the workspace sent this one: how the
//! engine's renderings name the sender and carry the body, for the
//! labelled block the transcript shows ([`super::Block::Peer`]).
//!
//! The engine renders an incoming peer message for the model as
//! `Peer message from <sender>:\n<body>` (meerkat-core
//! `format_peer_message_projection`), the sender being the sending
//! member's comms name, `<mob>/<profile>/<agent>`. A run the message
//! started carries that text in its input, framed by the notice's summary
//! (`Peer message`) and its `Delivery ID:` line.

/// The engine's header of a peer message's body.
const HEADER: &str = "Peer message from ";
/// The notice summary the engine puts first in a peer message's run input.
const SUMMARY: &str = "Peer message\n";

/// The agent's own name in a comms name (`tui/main/main-ab12` →
/// `main-ab12`). An agent of another workspace keeps its workspace
/// (`other/main-ab12`, the name its wire gives it,
/// `crate::daemon::protocol::wire_label`): its name alone would read as an
/// agent of this one.
#[must_use]
pub fn short_name(comms_name: &str) -> &str {
    if comms_name.matches('/').count() < 2 {
        return comms_name;
    }
    comms_name.rsplit('/').next().unwrap_or(comms_name)
}

/// The body of a peer message from `from` (its comms name) in the engine's
/// rendering `text`: the text after the last `Peer message from <from>:`
/// header, up to the notice's `Delivery ID:` trailer. Text without the
/// header is returned trimmed, as it is.
#[must_use]
pub fn body(from: &str, text: &str) -> String {
    let header = format!("{HEADER}{from}:\n");
    let rest = match text.rfind(&header) {
        Some(at) => &text[at + header.len()..],
        None => text,
    };
    let rest = rest.split("\nDelivery ID:").next().unwrap_or(rest);
    let rest = rest.trim_end();
    rest.strip_suffix("\nPeer message")
        .unwrap_or(rest)
        .trim()
        .to_string()
}

/// The peer messages a run input holds, each with its sender (its comms
/// name) and body, in order (`None`: another input, a prompt). One input
/// carries several when they reached a busy agent together. The input
/// must open with the engine's framing (the summary, then the header): a
/// prompt that quotes a peer message further in is a prompt.
///
/// Each message is framed as the summary line `Peer message`, the header
/// (once or twice), the body, then `Peer message` and its `Delivery ID:`
/// (and `In reply to:`) lines. A header only starts a message right after
/// the summary line or at the input's start, so a body that quotes one
/// stays whole.
#[must_use]
pub fn of_inputs(text: &str) -> Option<Vec<(String, String)>> {
    const SUMMARY_LINE: &str = "Peer message";
    let opening = text.strip_prefix(SUMMARY).unwrap_or(text);
    if !opening.starts_with(HEADER) {
        return None;
    }
    let sender = |line: &str| {
        line.strip_prefix(HEADER)
            .and_then(|rest| rest.strip_suffix(':'))
            .filter(|from| !from.is_empty())
            .map(str::to_string)
    };
    let trailer = |line: &str| line.starts_with("Delivery ID:") || line.starts_with("In reply to:");
    let lines: Vec<&str> = text.lines().collect();
    let mut messages: Vec<(String, Vec<&str>)> = Vec::new();
    let mut open = false;
    for (i, line) in lines.iter().copied().enumerate() {
        let before = i.checked_sub(1).and_then(|at| lines.get(at)).copied();
        let after = lines.get(i + 1).copied();
        if line == SUMMARY_LINE && after.is_none_or(|next| trailer(next) || sender(next).is_some())
        {
            open = false;
            continue;
        }
        if !open && trailer(line) {
            continue;
        }
        if let Some(from) = sender(line) {
            let repeats = matches!(messages.last(), Some((last, body)) if open && *last == from && body.is_empty());
            if repeats {
                continue;
            }
            if i == 0 || before == Some(SUMMARY_LINE) {
                messages.push((from, Vec::new()));
                open = true;
                continue;
            }
        }
        if open && let Some((_, body)) = messages.last_mut() {
            body.push(line);
        }
    }
    (!messages.is_empty()).then(|| {
        messages
            .into_iter()
            .map(|(from, body)| (from, body.join("\n").trim().to_string()))
            .collect()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The run input of a peer message as the engine sends it (observed
    /// on the engine: the summary, the header twice, the body, the summary and
    /// the delivery id).
    const INPUT: &str = "Peer message\nPeer message from tui/main/main-gak7:\nPeer message from tui/main/main-gak7:\nI am changing src/lib.rs\nPeer message\nDelivery ID: e9633901-e3a8-425c-8217-048ed236b907";

    #[test]
    fn a_peer_message_input_names_its_sender_and_body() {
        let (from, body) = of_inputs(INPUT).unwrap().remove(0);
        assert_eq!(from, "tui/main/main-gak7");
        assert_eq!(short_name(&from), "main-gak7");
        assert_eq!(body, "I am changing src/lib.rs");
        // A committed notice's content.
        assert_eq!(
            super::body(
                &from,
                "Peer message from tui/main/main-gak7:\nline one\nline two"
            ),
            "line one\nline two"
        );
        // A prompt is no peer message.
        assert_eq!(of_inputs("fix the failing test"), None);
        // Nor is one that quotes a peer message (another client's prompt was labelled as a message from the
        // agent it quoted).
        assert_eq!(
            of_inputs(&format!("why did you answer this?\n\n{INPUT}")),
            None
        );
        assert_eq!(
            of_inputs("see: Peer message from tui/main/main-gak7:\nhello"),
            None
        );
        assert_eq!(short_name("main-ab12"), "main-ab12");
        // Another workspace's agent keeps its workspace.
        assert_eq!(short_name("other/main-ab12"), "other/main-ab12");
        let (from, body) =
            of_inputs("Peer message\nPeer message from other/main-ab12:\nhello\nPeer message")
                .unwrap()
                .remove(0);
        assert_eq!(
            (short_name(&from), body.as_str()),
            ("other/main-ab12", "hello")
        );
    }

    /// Several messages that reached a busy agent together come as one
    /// input: each is found with its sender and its own body, without the
    /// framing lines, and a body that quotes a header stays whole.
    #[test]
    fn an_input_of_several_peer_messages_gives_each() {
        let input = "Peer message\n\
Peer message from tui/main/core:\n\
Peer message from tui/main/core:\n\
done in four files.\n\
it said: Peer message from tui/main/x:\n\
second paragraph\n\
Peer message\n\
Delivery ID: 58cc392a-9247-4bd4-8126-e72ca0bd035e\n\
In reply to: b79b8339-6cdf-4918-9fdb-ee7852ba7ea6\n\
Peer message\n\
Peer message from tui/main/review:\n\
Peer message from tui/main/review:\n\
verified\n\
Peer message\n\
Delivery ID: 8168d5d2-c1c7-4595-8046-d12650541043\n\
Peer message\n\
Peer message from tui/main/core:\n\
no further edits\n\
Peer message\n\
Delivery ID: cfee210d-090a-417b-8247-e988e90f6bdd\n\
In reply to: af3b0e3b-75f4-4b10-92ba-7470e58d571b";
        let messages = of_inputs(input).unwrap();
        assert_eq!(
            messages,
            vec![
                (
                    "tui/main/core".to_string(),
                    "done in four files.\nit said: Peer message from tui/main/x:\nsecond paragraph"
                        .to_string()
                ),
                ("tui/main/review".to_string(), "verified".to_string()),
                ("tui/main/core".to_string(), "no further edits".to_string()),
            ]
        );
    }
}
