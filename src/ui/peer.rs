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
/// `main-ab12`).
#[must_use]
pub fn short_name(comms_name: &str) -> &str {
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

/// The sender (its comms name) and body of a run input that is a peer
/// message (`None`: another input, a prompt). The input must open with
/// the engine's framing (the summary, then the header): a prompt that
/// quotes a peer message further in is a prompt.
#[must_use]
pub fn of_input(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix(SUMMARY).unwrap_or(text);
    let rest = rest.strip_prefix(HEADER)?;
    let from = rest.split(":\n").next()?.to_string();
    if from.is_empty() || from.contains('\n') {
        return None;
    }
    Some((from.clone(), body(&from, text)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The run input of a peer message as the engine sends it (observed
    /// on 0.9.11: the summary, the header twice, the body, the summary and
    /// the delivery id).
    const INPUT: &str = "Peer message\nPeer message from tui/main/main-gak7:\nPeer message from tui/main/main-gak7:\nI am changing src/lib.rs\nPeer message\nDelivery ID: e9633901-e3a8-425c-8217-048ed236b907";

    #[test]
    fn a_peer_message_input_names_its_sender_and_body() {
        let (from, body) = of_input(INPUT).unwrap();
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
        assert_eq!(of_input("fix the failing test"), None);
        // Nor is one that quotes a peer message (another client's prompt was labelled as a message from the
        // agent it quoted).
        assert_eq!(
            of_input(&format!("why did you answer this?\n\n{INPUT}")),
            None
        );
        assert_eq!(
            of_input("see: Peer message from tui/main/main-gak7:\nhello"),
            None
        );
        assert_eq!(short_name("main-ab12"), "main-ab12");
    }
}
