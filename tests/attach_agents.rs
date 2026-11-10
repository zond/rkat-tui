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
    let connection = daemon.connect_bound().await;
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

/// The peer id of the agent `name` in the last `peers` result of
/// `messages`: the id of the entry naming it.
fn peer_id_of(messages: &[Message], name: &str) -> Option<String> {
    fn find(value: &serde_json::Value, name: &str) -> Option<String> {
        match value {
            serde_json::Value::Object(map) => {
                let names_it = map
                    .values()
                    .any(|v| v.as_str().is_some_and(|s| s.contains(name)));
                if names_it && let Some(id) = map.get("peer_id").and_then(|v| v.as_str()) {
                    return Some(id.to_string());
                }
                map.values().find_map(|v| find(v, name))
            }
            serde_json::Value::Array(items) => items.iter().find_map(|v| find(v, name)),
            _ => None,
        }
    }
    messages.iter().rev().find_map(|m| match m {
        Message::ToolResults { results, .. } => results.iter().find_map(|r| {
            let text = r.text_content();
            serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| find(&value, name))
        }),
        _ => None,
    })
}

/// A sends B a message with its `send_message` tool (B's peer id from its
/// `peers` tool, which lists the other agent and no one else): B, idle,
/// wakes for it, its client shows the message labelled with A's name, and
/// B's turn sees it; the gate still judges what B does about it. Each agent
/// was told about the other (the persona names it; a note at its join).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_message_wakes_the_other_agent_and_its_client_shows_it() {
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::{Effect, Msg};
    let ws = Workspace::new();
    let b_name = Arc::new(parking_lot::Mutex::new(String::new()));
    let send = {
        let b_name = Arc::clone(&b_name);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let peer = peer_id_of(messages, &b_name.lock()).unwrap_or_else(|| "none".into());
            Step::Tool {
                id: "m1",
                name: "send_message",
                args: json!({"peer_id": peer, "body": "I am changing src/lib.rs", "handling_mode": "queue"}),
            }
        }))
    };
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "p1",
            name: "peers",
            args: json!({}),
        },
        send,
        Step::Text("told b"),
        Step::Tool {
            id: "s1",
            name: "shell",
            args: json!({"command": "python3 -c 'print(1)'"}),
        },
        Step::Text("noted"),
    ]);
    let classifier = rig::ScriptedClassifier::allowing(Vec::new());
    let setup = rig::Setup::default().classifier(Arc::clone(&classifier) as _);
    let args = setup.command_line(&ws.root);
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let mut a = rig::Client::attach(&ws.root).await;
    let mut b = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    let a_name = a.agent().await;
    *b_name.lock() = b.agent().await;
    let b_name = b_name.lock().clone();
    a.send(Effect::StartTurn("tell b".into()));
    assert_eq!(a.turn_finished().await.result.unwrap().text, "told b");
    // A's peers: B, and only B.
    let peers = tool_results(&llm.requests()[1]);
    assert!(peers.contains(&format!("tui/main/{b_name}")), "{peers}");
    assert_eq!(peers.matches("\"peer_id\"").count(), 1, "{peers}");
    // B woke for the message: its client shows it, labelled.
    b.wait_for(|m| {
        matches!(m, Msg::Peer { from, text } if *from == a_name && text == "I am changing src/lib.rs")
    })
    .await;
    assert_eq!(b.turn_finished().await.result.unwrap().text, "noted");
    let seen = llm.recorded();
    let b_turn = &seen[3];
    assert!(
        format!("{:?}", b_turn.messages).contains("I am changing src/lib.rs"),
        "B's turn sees the message"
    );
    // Told about each other: B's persona names it, and a note names A.
    let system = format!("{:?}", b_turn.messages);
    assert!(
        system.contains(&format!("You are agent `{b_name}`")),
        "{system}"
    );
    assert!(
        system.contains(&format!(
            "Other agents working in this workspace now: `{a_name}`"
        )),
        "{system}"
    );
    let a_system = format!("{:?}", seen[0].messages);
    assert!(
        a_system.contains(&format!("Agent `{b_name}` joined this workspace")),
        "{a_system}"
    );
    // The gate judged B's call (the classifier reviewed it).
    assert!(
        classifier.tools().iter().any(|tool| tool == "shell"),
        "{:?}",
        classifier.tools()
    );
    a.quit().await;
    b.quit().await;
    daemon.stop().await;
}

/// B quits (retires): A is told it left at its next turn, and a message A
/// sends B with the peer id it had is not delivered; nothing runs for B.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quit_agent_is_announced_and_takes_no_more_messages() {
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::{Departure, Effect, Msg};
    let ws = Workspace::new();
    let b_name = Arc::new(parking_lot::Mutex::new(String::new()));
    let send = {
        let b_name = Arc::clone(&b_name);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let peer = peer_id_of(messages, &b_name.lock()).unwrap_or_else(|| "none".into());
            Step::Tool {
                id: "m2",
                name: "send_message",
                args: json!({"peer_id": peer, "body": "are you there?", "handling_mode": "queue"}),
            }
        }))
    };
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "p1",
            name: "peers",
            args: json!({}),
        },
        Step::Text("listed"),
        send,
        Step::Text("tried"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = rig::Client::attach(&ws.root).await;
    let mut b = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    *b_name.lock() = b.agent().await;
    let name = b_name.lock().clone();
    a.send(Effect::StartTurn("who is here".into()));
    assert_eq!(a.turn_finished().await.result.unwrap().text, "listed");
    b.send(Effect::Leave(Departure::Retire));
    b.wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.contains("retired")))
        .await;
    b.quit().await;
    a.send(Effect::StartTurn("ask b".into()));
    assert_eq!(a.turn_finished().await.result.unwrap().text, "tried");
    let seen = llm.recorded();
    let told = format!("{:?}", seen[2].messages);
    assert!(
        told.contains(&format!("Agent `{name}` left this workspace")),
        "{told}"
    );
    // The engine's refusal: the retired agent is no trusted peer now.
    let result = tool_results(&seen[3].messages);
    assert!(
        result.contains("peer_not_found_or_not_trusted"),
        "the send was refused: {result}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(llm.recorded().len(), 4, "nothing ran for the retired agent");
    a.quit().await;
    daemon.stop().await;
}
