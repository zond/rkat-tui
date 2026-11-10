//! Several agents per workspace (P5, `docs/mob-split.md`): the agents'
//! names are the daemon's, and each client works with its own agent.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::Running;
use serde_json::json;

/// The text of every tool result in `messages`.
fn tool_results(messages: &[Message]) -> String {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResults { results, .. } => Some(
                results
                    .iter()
                    .map(|r| r.text_content())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An agent cannot seat a member under an agent's name: the names are the
/// daemon's (a member so named would be shaped as an agent without being
/// one of the daemon's). Another name is seated as a helper as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_cannot_seat_a_member_under_an_agents_name() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if tool_results(messages).is_empty() {
            Step::Tool {
                id: "s1",
                name: "spawn_member",
                args: json!({
                    "profile": "main",
                    "member_id": "main-zz9z",
                    "initial_message": "pretend to be an agent",
                }),
            }
        } else {
            Step::Text("done")
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step.clone(), step]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect().await;
    connection
        .call(
            "mob/turn_start",
            json!({"mob_id": "tui", "agent_identity": daemon.agent, "prompt": "seat one"}),
        )
        .await
        .unwrap();
    let requests = llm.requests();
    let results = tool_results(requests.last().unwrap());
    assert!(
        results.contains("reserved for the workspace's agents"),
        "{results}"
    );
    let members = connection
        .call("mob/members", json!({"mob_id": "tui"}))
        .await
        .unwrap();
    assert!(!members.to_string().contains("main-zz9z"), "{members}");
    let status = daemon.status().await;
    assert_eq!(status.agents.len(), 1, "{status:?}");
    drop(connection);
    daemon.stop().await;
}

/// Two clients on two agents each see their own agent; `/agents` lists
/// both, and `/agents <name>` moves a client to the other agent, whose
/// conversation it then shows (the agent it left keeps running).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_lists_the_agents_and_switches_to_one() {
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::{Block, Effect, Msg};
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("a answers"), Step::Text("b answers")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = rig::Client::attach(&ws.root).await;
    let mut b = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    let (a_name, b_name) = (a.agent().await, b.agent().await);
    assert_ne!(a_name, b_name);
    a.send(Effect::StartTurn("for a".into()));
    assert_eq!(a.turn_finished().await.result.unwrap().text, "a answers");
    b.send(Effect::StartTurn("for b".into()));
    assert_eq!(b.turn_finished().await.result.unwrap().text, "b answers");
    // Each client saw only its own agent's turn.
    assert_eq!(a.texts("b answers"), 0);
    assert_eq!(b.texts("a answers"), 0);
    a.send(Effect::Agents(None));
    let listing = a
        .wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("2 agents")))
        .await;
    let Msg::Notice { text, .. } = listing else {
        unreachable!()
    };
    assert!(
        text.contains(&format!("{a_name} · idle")) && text.contains("(this rkat-tui)"),
        "{text}"
    );
    assert!(text.contains(&format!("{b_name} · idle")), "{text}");
    // An agent that is not here is refused with the ones that are.
    a.send(Effect::Agents(Some("main-nope".into())));
    a.wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.contains("no agent main-nope")))
        .await;
    a.send(Effect::Agents(Some(b_name.clone())));
    a.wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.contains(&format!("switched to agent {b_name}"))))
        .await;
    a.wait_for(|m| {
        matches!(m, Msg::ResetTranscript(blocks)
            if blocks.iter().any(|b| matches!(b, Block::User { text } if text == "for b")))
    })
    .await;
    assert_eq!(a.agent().await, b_name);
    let status = daemon.status().await;
    let row = |name: &str| {
        status
            .agents
            .iter()
            .find(|row| row.name == name)
            .cloned()
            .unwrap()
    };
    assert_eq!(row(&b_name).clients, 2, "{status:?}");
    let left = row(&a_name);
    assert_eq!(left.clients, 0, "{status:?}");
    assert!(left.detached, "the agent left keeps running: {status:?}");
    a.quit().await;
    b.quit().await;
    daemon.stop().await;
}
