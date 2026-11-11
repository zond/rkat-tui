//! rkat-tui's own search and reveal (`src/daemon/deferral`) on the rig:
//! a fresh conversation's requests leave the schedule tools and the agent
//! mob tools other than `delegate` out; `tool_search` reveals exactly its
//! matches for that conversation from its next request on, kept across a
//! daemon restart; `/new` starts with nothing revealed; another agent of
//! the same daemon and a `delegate` helper are not affected.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/rig.rs"]
mod rig;

use std::sync::Arc;

use common::{Recorded, ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::{Attached, Client, Running};
use rkat_tui::approval::mode::PermissionMode;
use rkat_tui::daemon::deferral::reveal::TOOL_SEARCH;
use rkat_tui::daemon::protocol::AgentChoice;
use rkat_tui::ui::{Effect, Msg};
use serde_json::json;

/// The text of the tool result for call `id` in `messages`.
fn tool_result(messages: &[Message], id: &str) -> String {
    messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResults { results, .. } => results
                .iter()
                .find(|r| r.tool_use_id == id)
                .map(meerkat_core::types::ToolResult::text_content),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

/// The agent's call creating a one-shot schedule `name` on its own
/// session, due `in_secs` after the call is made.
fn create_at_call(
    id: &'static str,
    name: &'static str,
    in_secs: i64,
    prompt: &'static str,
) -> Step {
    Step::Dynamic(Arc::new(move |_: &[Message]| {
        let due = chrono::Utc::now() + chrono::Duration::seconds(in_secs);
        Step::Tool {
            id,
            name: "meerkat_schedule_create",
            args: json!({
                "name": name,
                "trigger": {"type": "once", "due_at_utc": due.to_rfc3339()},
                "target": {"target_kind": "session", "type": "current_session",
                           "action": {"type": "prompt", "prompt": prompt}}
            }),
        }
    }))
}

/// The search step revealing `meerkat_schedule_create` alone.
fn search_create(id: &'static str) -> Step {
    Step::Tool {
        id,
        name: TOOL_SEARCH,
        args: json!({"query": "meerkat_schedule_create"}),
    }
}

/// The deferrable tools `request` declares.
fn hidden_in(daemon: &Running, request: &Recorded) -> Vec<String> {
    let deferrable = daemon.deferral().deferrable.clone();
    request
        .defs
        .iter()
        .filter(|def| deferrable.contains(def.name.as_str()))
        .map(|def| def.name.to_string())
        .collect()
}

/// A fresh conversation's request declares `delegate` and the search tool
/// but no schedule tool and no other agent mob tool; a search for
/// `create` in the schedule family answers that one tool and reveals
/// exactly it from the next request on; the agent calls it, the schedule
/// fires and runs the agent's turn, whose request still declares it. A
/// search passes the gate in plan mode without asking anyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_search_reveals_its_matches_and_the_schedule_fires() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        search_create("q1"),
        create_at_call("s1", "check", 2, "check the build"),
        Step::Text("scheduled"),
        Step::Text("checked"),
        Step::Tool {
            id: "q2",
            name: TOOL_SEARCH,
            args: json!({"family": "mob"}),
        },
        Step::Text("planned"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("remind me".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "scheduled");
    let recorded = llm.recorded();
    let first = &recorded[0];
    assert!(
        hidden_in(&rig.daemon, first).is_empty(),
        "{:?}",
        first.tools
    );
    assert!(
        first.tools.iter().any(|t| t == "delegate"),
        "{:?}",
        first.tools
    );
    assert!(
        first.tools.iter().any(|t| t == TOOL_SEARCH),
        "{:?}",
        first.tools
    );
    let found = tool_result(&recorded[1].messages, "q1");
    assert!(found.contains("meerkat_schedule_create"), "{found}");
    assert!(!found.contains("meerkat_schedule_list"), "{found}");
    assert_eq!(
        hidden_in(&rig.daemon, &recorded[1]),
        ["meerkat_schedule_create"]
    );
    // The request grew by that one definition.
    let create = recorded[1]
        .defs
        .iter()
        .find(|def| def.name == "meerkat_schedule_create")
        .unwrap();
    assert_eq!(
        recorded[1].total_tool_bytes() - first.total_tool_bytes(),
        common::tool_def_bytes(create)
    );
    eprintln!(
        "one revealed tool: +{} bytes",
        common::tool_def_bytes(create)
    );
    let created = tool_result(&recorded[2].messages, "s1");
    assert!(created.contains("schedule_id"), "{created}");
    rig.wait_for(|m| matches!(m, Msg::Triggered { label, .. } if label == "scheduled: check"))
        .await;
    assert_eq!(rig.turn_finished().await.unwrap().text, "checked");
    assert_eq!(
        hidden_in(&rig.daemon, &llm.recorded()[3]),
        ["meerkat_schedule_create"]
    );
    // Plan mode: the search is read-only.
    rig.set_mode(PermissionMode::Plan).await;
    rig.send(Effect::StartTurn("plan a team".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "planned");
    let recorded = llm.recorded();
    let found = tool_result(&recorded[5].messages, "q2");
    assert!(
        found.contains("council") && !found.contains("delegate"),
        "{found}"
    );
    assert!(hidden_in(&rig.daemon, &recorded[5]).contains(&"council".to_string()));
    assert_eq!(rig.escalations(), 0);
    rig.quit().await;
}

/// What one agent reveals is its conversation's alone: another agent of
/// the same daemon still declares no hidden tool, while the first keeps
/// its reveal in its next turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_agent_reveals_nothing() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        search_create("q1"),
        Step::Text("a1"),
        Step::Text("b1"),
        Step::Text("a2"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = Client::attach(&ws.root).await;
    a.send(Effect::StartTurn("find it".into()));
    assert_eq!(a.turn_finished().await.unwrap().text, "a1");
    let mut b = Client::attach_agent(&ws.root, AgentChoice::New).await;
    assert_ne!(a.agent().await, b.agent().await);
    b.send(Effect::StartTurn("hello".into()));
    assert_eq!(b.turn_finished().await.unwrap().text, "b1");
    a.send(Effect::StartTurn("again".into()));
    assert_eq!(a.turn_finished().await.unwrap().text, "a2");
    let recorded = llm.recorded();
    assert_eq!(
        hidden_in(&daemon, &recorded[1]),
        ["meerkat_schedule_create"]
    );
    assert!(
        hidden_in(&daemon, &recorded[2]).is_empty(),
        "{:?}",
        recorded[2].tools
    );
    assert!(recorded[2].tools.iter().any(|t| t == TOOL_SEARCH));
    assert_eq!(
        hidden_in(&daemon, &recorded[3]),
        ["meerkat_schedule_create"]
    );
    b.quit().await;
    a.quit().await;
    daemon.stop().await;
}

/// A reveal is kept in the realm and survives a daemon restart (the
/// restored conversation still declares the tool); `/new` starts a
/// conversation with nothing revealed, and `/resume` brings the first
/// conversation back with its reveal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reveal_survives_a_restart_and_new_clears_it() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        search_create("q1"),
        Step::Text("found"),
        Step::Text("restored"),
        Step::Text("fresh"),
        Step::Text("resumed"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("find it".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "found");
    client.quit().await;
    daemon.stop().await;
    let stored = std::fs::read_to_string(
        common::realm_of(&ws.root).join(rkat_tui::daemon::deferral::reveal::REVEALS_FILE),
    )
    .unwrap();
    assert!(stored.contains("meerkat_schedule_create"), "{stored}");

    let daemon = Running::start(&ws.root, &llm).await;
    assert!(!daemon.created);
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("still there?".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "restored");
    assert_eq!(
        hidden_in(&daemon, &llm.recorded()[2]),
        ["meerkat_schedule_create"]
    );
    client.send(Effect::NewSession);
    client
        .saw(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("a new conversation")))
        .await;
    client.send(Effect::StartTurn("fresh start".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "fresh");
    let fresh = &llm.recorded()[3];
    assert!(hidden_in(&daemon, fresh).is_empty(), "{:?}", fresh.tools);
    assert!(fresh.tools.iter().any(|t| t == TOOL_SEARCH));
    client.send(Effect::ResumeSession(daemon.session.to_string()));
    client
        .saw(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("resumed conversation")))
        .await;
    client.send(Effect::StartTurn("back again".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "resumed");
    assert_eq!(
        hidden_in(&daemon, &llm.recorded()[4]),
        ["meerkat_schedule_create"]
    );
    client.quit().await;
    daemon.stop().await;
}

/// A model that calls a hidden tool by a guessed name without searching
/// gets the engine's "not found" (the search tool's description says a
/// hidden tool is not found until searched for), and nothing is revealed
/// by it; the search then works as usual.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_guessed_hidden_tool_is_not_found_until_searched() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "g1",
            name: "meerkat_schedule_list",
            args: json!({}),
        },
        Step::Tool {
            id: "q1",
            name: TOOL_SEARCH,
            args: json!({"query": "meerkat_schedule_list"}),
        },
        Step::Tool {
            id: "l1",
            name: "meerkat_schedule_list",
            args: json!({}),
        },
        Step::Text("listed"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("list my schedules".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "listed");
    let recorded = llm.recorded();
    let guessed = tool_result(&recorded[1].messages, "g1");
    assert!(guessed.contains("not found"), "{guessed}");
    assert!(hidden_in(&rig.daemon, &recorded[1]).is_empty());
    let search = recorded[0]
        .defs
        .iter()
        .find(|def| def.name == TOOL_SEARCH)
        .unwrap();
    assert!(
        search.description.contains("not found"),
        "{}",
        search.description
    );
    assert_eq!(
        hidden_in(&rig.daemon, &recorded[2]),
        ["meerkat_schedule_list"]
    );
    let listed = tool_result(&recorded[3].messages, "l1");
    assert!(!listed.contains("not found"), "{listed}");
    assert_eq!(rig.escalations(), 0);
    rig.quit().await;
}
