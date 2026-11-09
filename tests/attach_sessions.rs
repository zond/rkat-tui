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
    // Resuming the current one is a no-op (`last` names the most recent
    // other one, as in-process).
    client.send(Effect::ResumeSession(first.clone()));
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

/// The daemon itself refuses a switch while the orchestrator's run is
/// open: the client's guard is in another process (another client's turn).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_switch_is_refused_while_another_clients_turn_runs() {
    let ws = Workspace::new();
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![Step::Gated(
        std::sync::Arc::clone(&gate),
        Box::new(Step::Text("finished")),
    )]);
    let daemon = Running::start(&ws.root, &llm).await;
    let running = daemon.connect().await;
    running
        .call(
            "mob/member_send",
            json!({"mob_id": "tui", "agent_identity": "main", "content": "long work"}),
        )
        .await
        .unwrap();
    llm.wait_call(0).await;
    let other = daemon.connect().await;
    let refused = other.call("rkat/new", json!({})).await.unwrap_err();
    assert!(
        matches!(&refused, rkat_tui::client::ClientError::Rpc { code, message, .. }
            if *code == rkat_tui::daemon::protocol::SWITCH_BUSY && message.contains("interrupt")),
        "{refused}"
    );
    // The turn was not cut off: it finishes on the same session.
    gate.notify_one();
    let status = other
        .call(
            "mob/member_status",
            json!({"mob_id": "tui", "agent_identity": "main"}),
        )
        .await
        .unwrap();
    assert_eq!(
        status["current_session_id"],
        json!(daemon.session.to_string())
    );
    // Once the run has ended the switch goes through.
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    loop {
        match other.call("rkat/new", json!({})).await {
            Ok(answer) => {
                assert_ne!(answer["session_id"], json!(daemon.session.to_string()));
                break;
            }
            Err(rkat_tui::client::ClientError::Rpc { code, .. })
                if code == rkat_tui::daemon::protocol::SWITCH_BUSY
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(err) => panic!("{err}"),
        }
    }
    drop((running, other));
    daemon.stop().await;
}

/// The token counter and the context gauge follow the conversation: after
/// `/resume` they show the resumed conversation's own usage (the daemon's
/// `rkat/usage`, the last turn's context), after `/new` nothing, and a TUI
/// that attaches later starts from the session's totals and its last
/// context, not from nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_token_counter_and_gauge_follow_the_conversation() {
    use rkat_tui::ui::StatusUpdate;
    let tokens = |m: &Msg| match m {
        Msg::Status(StatusUpdate::Tokens { input, output }) => Some((*input, *output)),
        _ => None,
    };
    let gauge = |m: &Msg| match m {
        Msg::Status(StatusUpdate::Context { used, .. }) => Some(*used),
        _ => None,
    };
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::TextUsage("one", 100), Step::TextUsage("two", 7)]);
    let daemon = Running::start(&ws.root, &llm).await;
    let first = daemon.session.to_string();
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("first".into()));
    client.turn_finished().await.result.unwrap();

    client.send(Effect::NewSession);
    client.wait_for(|m| reset(m).is_some()).await;
    assert_eq!(
        tokens(&client.wait_for(|m| tokens(m).is_some()).await),
        Some((0, 0))
    );
    assert_eq!(
        gauge(&client.wait_for(|m| gauge(m).is_some()).await),
        Some(None)
    );
    client.send(Effect::StartTurn("second".into()));
    client.turn_finished().await.result.unwrap();

    client.send(Effect::ResumeSession(first));
    client
        .wait_for(|m| reset(m).is_some_and(|blocks| !blocks.is_empty()))
        .await;
    // Each scripted call reports its input and 5 output tokens.
    assert_eq!(
        tokens(&client.wait_for(|m| tokens(m).is_some()).await),
        Some((100, 5))
    );
    assert_eq!(
        gauge(&client.wait_for(|m| gauge(m).is_some()).await),
        Some(Some(105))
    );

    // A TUI attaching now starts from the resumed conversation's totals.
    let mut late = Client::attach(&ws.root).await;
    late.saw(|m| tokens(m) == Some((100, 5))).await;
    // Its cursor is past the last turn (the replay holds no turn end): the
    // gauge is seeded from the events before it, as a switch seeds it.
    late.saw(|m| gauge(m) == Some(Some(105))).await;
    late.quit().await;
    client.quit().await;
    daemon.stop().await;
}

/// `/compact now` in attach mode asks the orchestrator's next compaction
/// check to compact (`mob/compact_member`): the next prompt's first model
/// call is the summary, and the client sees `compaction_completed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compact_now_compacts_at_the_next_turn() {
    let ws = Workspace::new();
    let mut script = vec![Step::Text("ok"); 4];
    script.push(Step::Text("SUMMARY of the earlier turns"));
    script.push(Step::Text("after"));
    let llm = ScriptedLlmClient::new(script);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    for i in 0..4 {
        client.send(Effect::StartTurn(format!("turn {i}")));
        client.turn_finished().await.result.unwrap();
    }
    client.send(Effect::CompactNow);
    let asked = client
        .wait_for(|m| matches!(m, Msg::CompactRequested(_)))
        .await;
    assert!(matches!(asked, Msg::CompactRequested(Ok(()))), "{asked:?}");
    client.send(Effect::StartTurn("next".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "after");
    client
        .saw(|m| {
            matches!(
                m,
                Msg::Agent(envelope)
                    if matches!(envelope.payload, meerkat_core::AgentEvent::CompactionCompleted { .. })
            )
        })
        .await;
    assert_eq!(llm.recorded().len(), 6, "the summary call came first");
    client.quit().await;
    daemon.stop().await;
}
