//! Conversations in attach mode (P4): `/new` seats the daemon's
//! orchestrator on a fresh session, `/resume` lists the workspace's
//! conversations and brings an earlier one back (rendered from its
//! committed messages), another attached client follows the switch, and
//! `/rewind` says why it is not offered.

#![cfg(feature = "mob")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use common::{ScriptedLlmClient, Step, Workspace, user_texts};
use rig::{Client, Running};
use rkat_tui::client::sessions::REWIND_UNAVAILABLE;
use rkat_tui::ui::{Block, Effect, Msg};
use serde_json::json;

fn reset(msg: &Msg) -> Option<&Vec<Block>> {
    match msg {
        Msg::ResetTranscript(blocks) => Some(blocks),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_starts_fresh_and_resume_brings_the_old_conversation_back() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Text("one"),
        Step::Text("two"),
        Step::Text("three"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let first = daemon.session.to_string();
    let mut client = Client::attach(&ws.root).await;
    let mut other = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("first".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "one");

    // /new: an empty transcript, a fresh session for the next prompt.
    client.send(Effect::NewSession);
    let cleared = client.wait_for(|m| reset(m).is_some()).await;
    assert!(reset(&cleared).unwrap().is_empty());
    client
        .saw(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("a new conversation")))
        .await;
    client.send(Effect::StartTurn("second".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "two");
    assert_eq!(user_texts(&llm.recorded()[1].messages), ["second"]);
    // The other client followed the switch.
    other
        .wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("another client switched")))
        .await;

    // /resume: both conversations are listed; the first comes back with
    // its transcript.
    client.send(Effect::RequestResumePicker);
    let picker = client
        .wait_for(|m| matches!(m, Msg::ShowResumePicker(_)))
        .await;
    let Msg::ShowResumePicker(items) = picker else {
        unreachable!()
    };
    assert_eq!(items.len(), 2, "{items:?}");
    assert!(items[0].1.ends_with("(current)"), "{items:?}");
    assert_eq!(items[1].0, first);
    assert!(items[1].1.contains("first"), "{items:?}");
    client.send(Effect::ResumeSession(first.clone()));
    let restored = client
        .wait_for(|m| reset(m).is_some_and(|blocks| !blocks.is_empty()))
        .await;
    let blocks = reset(&restored).unwrap();
    assert!(
        blocks
            .iter()
            .any(|b| matches!(b, Block::User { text } if text == "first")),
        "{blocks:?}"
    );
    assert!(
        blocks
            .iter()
            .any(|b| matches!(b, Block::Assistant { md } if md == "one")),
        "{blocks:?}"
    );
    client
        .saw(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("resumed conversation")))
        .await;
    client.send(Effect::StartTurn("third".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "three");
    assert_eq!(user_texts(&llm.recorded()[2].messages), ["first", "third"]);
    // Resuming the current one is a no-op.
    client.send(Effect::ResumeSession("last".into()));
    client
        .wait_for(
            |m| matches!(m, Msg::Notice { text, .. } if text == "already in this conversation"),
        )
        .await;
    other.quit().await;
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rewind_says_why_it_is_not_offered() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("one")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("first".into()));
    client.turn_finished().await.result.unwrap();
    client.send(Effect::RequestRewindMenu);
    client
        .wait_for(|m| matches!(m, Msg::Notice { text, .. } if text == REWIND_UNAVAILABLE))
        .await;
    // The reason holds: the engine refuses a transcript fork of the
    // member's session.
    let connection = daemon.connect().await;
    let refused = connection
        .call(
            "session/fork_at",
            json!({"session_id": daemon.session.to_string(), "message_index": 0}),
        )
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("mob-owned session transcripts cannot be edited"),
        "{refused}"
    );
    client.quit().await;
    daemon.stop().await;
}
