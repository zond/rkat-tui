//! Several agents per workspace: the agents'
//! names are the daemon's, and each client works with its own agent.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

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

/// A member the agent seats with the operator tool `spawn_member` and
/// `auto_wire_parent` is wired to the agent both ways (so the two can
/// message each other) and stays after its first turn until retired. The
/// persona's hint says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_seated_with_auto_wire_parent_is_wired_to_the_agent() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let helper = messages
            .iter()
            .any(|m| matches!(m, Message::User(_)) && format!("{m:?}").contains("HELPER-KICKOFF"));
        if helper {
            Step::Text("helper idle")
        } else if tool_results(messages).is_empty() {
            Step::Tool {
                id: "s1",
                name: "spawn_member",
                args: json!({
                    "profile": "main",
                    "member_id": "scout",
                    "initial_message": "HELPER-KICKOFF wait",
                    "auto_wire_parent": true,
                }),
            }
        } else {
            Step::Text("done")
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 6]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    connection
        .call(
            "mob/turn_start",
            json!({"mob_id": "tui", "agent_identity": daemon.agent, "prompt": "seat one"}),
        )
        .await
        .unwrap();
    let wired = |members: &serde_json::Value, from: &str, to: &str| {
        members["members"].as_array().is_some_and(|all| {
            all.iter().any(|m| {
                m["agent_identity"] == from
                    && m["wired_to"]
                        .as_array()
                        .is_some_and(|peers| peers.iter().any(|p| p == to))
            })
        })
    };
    let members = common::within(async {
        loop {
            let members = connection
                .call("mob/members", json!({"mob_id": "tui"}))
                .await
                .unwrap();
            if wired(&members, &daemon.agent, "scout") {
                return members;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(wired(&members, "scout", &daemon.agent), "{members}");
    // Its first turn ran and ended; it is still a member, wired.
    let ran = |llm: &ScriptedLlmClient| {
        llm.requests().iter().any(|messages| {
            messages
                .iter()
                .any(|m| format!("{m:?}").contains("HELPER-KICKOFF"))
        })
    };
    common::within(async {
        while !ran(&llm) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    let after = common::within(async {
        loop {
            let members = connection
                .call("mob/members", json!({"mob_id": "tui"}))
                .await
                .unwrap();
            let idle = connection
                .call(
                    "mob/member_status",
                    json!({"mob_id": "tui", "agent_identity": "scout"}),
                )
                .await
                .unwrap();
            if idle["progress"]["run_state"] == "idle" {
                return (members, idle);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    assert_eq!(after.1["output_preview"], "helper idle", "{after:?}");
    assert!(wired(&after.0, "scout", &daemon.agent), "{after:?}");
    drop(connection);
    daemon.stop().await;
}

/// Two clients on two agents each see their own agent; `/agents` lists
/// both (in its picker), and `/agents <name>` moves a client to the other agent, whose
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
    assert_eq!(a.turn_finished().await.unwrap().text, "a answers");
    b.send(Effect::StartTurn("for b".into()));
    assert_eq!(b.turn_finished().await.unwrap().text, "b answers");
    // Each client saw only its own agent's turn.
    assert_eq!(a.texts("b answers"), 0);
    assert_eq!(b.texts("a answers"), 0);
    a.send(Effect::Agents(None));
    let listing = a.wait_for(|m| matches!(m, Msg::ShowAgentPicker(_))).await;
    let Msg::ShowAgentPicker(rows) = listing else {
        unreachable!()
    };
    let shown: Vec<(&str, &str, bool)> = rows
        .iter()
        .map(|row| (row.name.as_str(), row.run, row.mine))
        .collect();
    let mut expected = vec![
        (a_name.as_str(), "idle", true),
        (b_name.as_str(), "idle", false),
    ];
    expected.sort_unstable();
    assert_eq!(shown, expected);
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

/// A start with neither `-c` nor `--resume` gets a new agent whichever
/// daemon it names, and is told so when others work there: a plain start
/// beside an idle detached agent, and `--attach=<dir>` beside it and the
/// plain one's. The first start, alone with the daemon's agent, has no
/// other to name (the notice's text for that case, none, is
/// `others_notice`'s unit test; here the daemon lists it alone).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_agent_is_told_about_the_others_whichever_daemon_it_names() {
    use rkat_tui::ui::{Departure, Effect, Msg};
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let real = |args: &rkat_tui::Args| Some(rkat_tui::client::agent_choice(args));
    let plain = common::args_for(&ws.root);
    let endpoint = common::endpoint_file_for(&ws.root);
    let mut a =
        rig::Client::attach_config(&plain, None, common::no_lists(), &endpoint, real(&plain)).await;
    let a_name = a.agent().await;
    assert_eq!(
        a_name, daemon.agent,
        "the first start takes the daemon's agent"
    );
    let status = daemon.status().await;
    assert_eq!(
        status.agents.len(),
        1,
        "alone: no other to name: {status:?}"
    );
    a.send(Effect::Leave(Departure::Detach));
    a.quit().await;
    without_client(&daemon, &a_name).await;

    let mut b =
        rig::Client::attach_config(&plain, None, common::no_lists(), &endpoint, real(&plain)).await;
    let b_name = b.agent().await;
    assert_ne!(b_name, a_name, "a plain start gets a new agent");
    b.wait_for(|m| matches!(m, Msg::Notice { text, .. } if text == &format!(
        "started a new agent ({b_name}); 1 other works here, 1 idle without a client: /agents picks one (rkat-tui -c joins the most recently active instead of starting a new one)"
    )))
    .await;

    // `--attach=<dir>` chooses the daemon only: a new agent again.
    let at = format!("--attach={}", ws.root.display());
    let attach = common::args_with(&ws.root, &[&at]);
    let target = attach.attach.clone().unwrap();
    let mut config = rkat_tui::client::configure(&attach, target).await.unwrap();
    config.lister = common::no_lists();
    config.endpoint_files = vec![endpoint.clone()];
    let mut c = rig::Client::attach_with_config(config).await;
    let c_name = c.agent().await;
    assert!(c_name != a_name && c_name != b_name, "{c_name}");
    c.wait_for(|m| {
        matches!(m, Msg::Notice { text, .. } if text.starts_with(&format!(
            "started a new agent ({c_name}); 2 others work here, 1 idle without a client:"
        )))
    })
    .await;
    assert_eq!(daemon.status().await.agents.len(), 3);
    b.quit().await;
    c.quit().await;
    daemon.stop().await;
}

/// Wait until the daemon lists `name` with no client.
async fn without_client(daemon: &Running, name: &str) {
    common::within(async {
        while !daemon
            .status()
            .await
            .agents
            .iter()
            .any(|row| row.name == name && row.clients == 0)
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
}

/// `/agents` opens the picker over every agent (this client's marked, the
/// ones nobody works with retirable, the others with why not); its retire
/// refuses this client's own agent, an agent with a client and a running
/// one, and retires an idle detached one, whose conversation stays in
/// `/resume`; its "new agent" row seats a fresh agent for this client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agent_picker_lists_retires_idle_agents_and_seats_a_new_one() {
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::{Departure, Effect, Msg};
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Text("b answers"),
        Step::Gated(Arc::clone(&gate), Box::new(Step::Text("d answers"))),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = rig::Client::attach(&ws.root).await;
    let mut b = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    let mut c = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    let mut d = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    let (a_name, b_name, c_name, d_name) = (
        a.agent().await,
        b.agent().await,
        c.agent().await,
        d.agent().await,
    );
    // B: a conversation, then left idle and detached.
    b.send(Effect::StartTurn("for b".into()));
    assert_eq!(b.turn_finished().await.unwrap().text, "b answers");
    let b_session = daemon.session_of(&b_name).await;
    b.send(Effect::Leave(Departure::Detach));
    b.quit().await;
    without_client(&daemon, &b_name).await;
    // D: left detached while its turn runs.
    d.send(Effect::StartTurn("for d".into()));
    llm.wait_call(1).await;
    d.send(Effect::Leave(Departure::Detach));
    d.quit().await;
    without_client(&daemon, &d_name).await;

    a.send(Effect::Agents(None));
    let shown = a.wait_for(|m| matches!(m, Msg::ShowAgentPicker(_))).await;
    let Msg::ShowAgentPicker(entries) = shown else {
        unreachable!()
    };
    let entry = |name: &str| entries.iter().find(|e| e.name == name).unwrap().clone();
    assert_eq!(entries.len(), 4, "{entries:?}");
    assert!(entry(&a_name).mine && entry(&a_name).unretirable.is_some());
    let b_entry = entry(&b_name);
    assert!(
        !b_entry.mine && b_entry.detached && b_entry.clients == 0,
        "{b_entry:?}"
    );
    assert_eq!(b_entry.run, "idle");
    assert_eq!(b_entry.title.as_deref(), Some("for b"));
    assert_eq!(
        b_entry.conversation.as_deref(),
        Some(rkat_tui::text::short_id(&b_session.to_string()))
    );
    assert_eq!(b_entry.unretirable, None);
    assert_eq!(entry(&c_name).clients, 1);
    assert!(entry(&c_name).unretirable.is_some());
    assert!(entry(&d_name).unretirable.is_some());

    // The daemon refuses each agent in use, saying why, and keeps it.
    for (name, why) in [
        (&a_name, "own agent"),
        (&c_name, "has 1 client"),
        (&d_name, "running a turn"),
    ] {
        a.send(Effect::RetireAgent(name.clone()));
        a.wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.contains(&format!("{name} was not retired")) && text.contains(why)))
            .await;
        // The picker that asked shows the refusal too.
        a.wait_for(|m| matches!(m, Msg::AgentPickerRefused(text) if text.contains(why)))
            .await;
    }
    let names: Vec<String> = daemon
        .status()
        .await
        .agents
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(names.len(), 4, "{names:?}");

    // B retires; its conversation stays in /resume.
    a.send(Effect::RetireAgent(b_name.clone()));
    let short = rkat_tui::text::short_id(&b_session.to_string()).to_string();
    a.wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.contains(&format!("retired agent {b_name}")) && text.contains(&short)))
        .await;
    // The open picker gets the agents again, without B.
    let refreshed = a
        .wait_for(|m| matches!(m, Msg::RefreshAgentPicker(_)))
        .await;
    assert!(
        matches!(&refreshed, Msg::RefreshAgentPicker(rows) if rows.len() == 3 && rows.iter().all(|r| r.name != b_name)),
        "{refreshed:?}"
    );
    let status = daemon.status().await;
    assert!(status.agents.iter().all(|r| r.name != b_name), "{status:?}");
    a.send(Effect::Agents(None));
    let shown = a
        .wait_for(|m| matches!(m, Msg::ShowAgentPicker(rows) if rows.len() == 3))
        .await;
    assert!(matches!(shown, Msg::ShowAgentPicker(rows) if rows.iter().all(|r| r.name != b_name)));
    a.send(Effect::RequestResumePicker);
    let picker = a.wait_for(|m| matches!(m, Msg::ShowResumePicker(_))).await;
    let Msg::ShowResumePicker(items) = picker else {
        unreachable!()
    };
    assert!(
        items.iter().any(|(id, _)| id == &b_session.to_string()),
        "{items:?}"
    );

    // The "new agent" row: A works with a fresh agent; the one it left
    // keeps running.
    a.send(Effect::NewAgent);
    a.wait_for(
        |m| matches!(m, Msg::Notice { text, .. } if text.starts_with("switched to a new agent")),
    )
    .await;
    // The status bar names the agent before the notice says so.
    let fresh = a.agent().await;
    assert!(
        ![&a_name, &b_name, &c_name, &d_name].contains(&&fresh),
        "{fresh}"
    );
    let status = daemon.status().await;
    assert_eq!(status.agents.len(), 4, "{status:?}");
    assert!(
        status
            .agents
            .iter()
            .any(|r| r.name == a_name && r.clients == 0)
    );

    gate.notify_one();
    daemon.idle(&d_name).await;
    a.quit().await;
    c.quit().await;
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
    assert_eq!(a.turn_finished().await.unwrap().text, "told b");
    // A's peers: B, and only B.
    let peers = tool_results(&llm.requests()[1]);
    assert!(peers.contains(&format!("tui/main/{b_name}")), "{peers}");
    assert_eq!(peers.matches("\"peer_id\"").count(), 1, "{peers}");
    // B woke for the message: its client shows it, labelled.
    b.wait_for(|m| {
        matches!(m, Msg::Peer { from, text } if *from == a_name && text == "I am changing src/lib.rs")
    })
    .await;
    assert_eq!(b.turn_finished().await.unwrap().text, "noted");
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
            "Other agents in this workspace now: `{a_name}` (idle, a client attached)"
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

/// How many roster notes (the daemon's lists of the other agents and
/// their states) `messages` hold.
fn roster_notes(messages: &[Message]) -> usize {
    format!("{messages:?}")
        .matches("Other agents in this workspace now:")
        .count()
}

/// The owner's decision (2026-11-13): an agent is shown which of the others
/// are working, idle with a client, or idle with none, and told to announce
/// only to the first two. A joins working (its turn held at the model), B
/// idle with its client, C idle after its client detached: D, joining, is
/// told exactly that, and its instructions say whom to announce to. The
/// note is a snapshot sent on roster and attachment changes, never for a
/// run: over A's three runs no agent is sent another, and nothing runs for
/// C, which no note wakes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_roster_note_shows_who_works_and_is_not_sent_for_runs() {
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::{Departure, Effect};
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&gate), Box::new(Step::Text("a done"))),
        Step::Text("d answered"),
        Step::Text("a2"),
        Step::Text("a3"),
        Step::Text("d again"),
        Step::Text("b answered"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = rig::Client::attach(&ws.root).await;
    let mut b = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    let mut c = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    let a_name = a.agent().await;
    let b_name = b.agent().await;
    let c_name = c.agent().await;
    c.send(Effect::Leave(Departure::Detach));
    c.quit().await;
    common::within(async {
        while !daemon
            .status()
            .await
            .agents
            .iter()
            .any(|row| row.name == c_name && row.clients == 0)
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    a.send(Effect::StartTurn("work on".into()));
    llm.wait_call(0).await;
    let mut d = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    d.send(Effect::StartTurn("who is here".into()));
    assert_eq!(d.turn_finished().await.unwrap().text, "d answered");
    let d_first = llm.recorded()[1].messages.clone();
    let told = format!("{d_first:?}");
    assert!(
        told.contains(&format!("Other agents in this workspace now: {}", {
            let mut listed = [
                (a_name.clone(), "working"),
                (b_name.clone(), "idle, a client attached"),
                (c_name.clone(), "idle, no client attached"),
            ];
            listed.sort();
            listed
                .iter()
                .map(|(name, state)| format!("`{name}` ({state})"))
                .collect::<Vec<_>>()
                .join(", ")
        })),
        "{told}"
    );
    for line in [
        "that the latest such note shows working or with a client attached",
        "Do not message an agent it shows idle with no client attached",
    ] {
        assert!(told.contains(line), "{line}: {told}");
    }
    assert_eq!(roster_notes(&d_first), 1, "{told}");
    // A's runs: the one held, then two more.
    gate.notify_one();
    assert_eq!(a.turn_finished().await.unwrap().text, "a done");
    for text in ["a2", "a3"] {
        a.send(Effect::StartTurn("more".into()));
        assert_eq!(a.turn_finished().await.unwrap().text, text);
    }
    // A was told of B's, C's and D's joins and of C's detach, and nothing
    // for its own runs or D's.
    let a_last = llm.recorded()[3].messages.clone();
    assert_eq!(roster_notes(&a_last), 4, "{a_last:?}");
    // D: still its one note after A's runs.
    d.send(Effect::StartTurn("again".into()));
    assert_eq!(d.turn_finished().await.unwrap().text, "d again");
    assert_eq!(roster_notes(&llm.recorded()[4].messages), 1);
    // B: its own at its seat, C's join and detach, D's join.
    b.send(Effect::StartTurn("and you".into()));
    assert_eq!(b.turn_finished().await.unwrap().text, "b answered");
    let b_last = llm.recorded()[5].messages.clone();
    assert_eq!(roster_notes(&b_last), 4, "{b_last:?}");
    assert_eq!(llm.recorded().len(), 6, "nothing ran for C");
    a.quit().await;
    b.quit().await;
    d.quit().await;
    daemon.stop().await;
}

/// A6 (the user's decision, 2026-11-10): B's client detaches, A's message
/// wakes B. Nobody started B's turn and nobody is with B, so its `curl`
/// is denied as unattended without a classifier request (B reads why), and
/// its in-project write still runs. A client attaching to B mid-turn is
/// shown what B was denied, and B's next `curl` goes to the classifier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_woken_agent_with_no_client_is_denied_what_needs_review() {
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::{Departure, Effect, Msg};
    let ws = Workspace::new();
    let b_name = Arc::new(parking_lot::Mutex::new(String::new()));
    let send = {
        let b_name = Arc::clone(&b_name);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let peer = peer_id_of(messages, &b_name.lock()).unwrap_or_else(|| "none".into());
            Step::Tool {
                id: "m1",
                name: "send_message",
                args: json!({"peer_id": peer, "body": "fetch the release notes", "handling_mode": "queue"}),
            }
        }))
    };
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "p1",
            name: "peers",
            args: json!({}),
        },
        send,
        Step::Text("told b"),
        // B, woken by the message with nobody attached.
        Step::Tool {
            id: "b1",
            name: "shell",
            args: json!({"command": "curl -s http://127.0.0.1:9/notes"}),
        },
        Step::Tool {
            id: "b2",
            name: "write_file",
            args: json!({"path": "notes.md", "content": "release notes\n"}),
        },
        Step::Gated(
            Arc::clone(&gate),
            Box::new(Step::Tool {
                id: "b3",
                name: "shell",
                args: json!({"command": "curl -s http://127.0.0.1:9/notes2"}),
            }),
        ),
        Step::Text("fetched"),
    ]);
    let classifier = rig::ScriptedClassifier::allowing(Vec::new());
    let setup = rig::Setup::default().classifier(Arc::clone(&classifier) as _);
    let args = setup.command_line(&ws.root);
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let mut a = rig::Client::attach(&ws.root).await;
    let mut b = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    *b_name.lock() = b.agent().await;
    let b_name = b_name.lock().clone();
    b.send(Effect::Leave(Departure::Detach));
    b.quit().await;
    common::within(async {
        loop {
            let status = daemon.status().await;
            if status
                .agents
                .iter()
                .any(|row| row.name == b_name && row.clients == 0)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    a.send(Effect::StartTurn("tell b".into()));
    assert_eq!(a.turn_finished().await.unwrap().text, "told b");
    // B's third model call waits at the gate: its first two calls are
    // decided.
    llm.wait_call(5).await;
    let reviewed_curls = || {
        classifier
            .seen()
            .iter()
            .filter(|(tool, args, _)| tool == "shell" && args.to_string().contains("curl"))
            .count()
    };
    assert_eq!(reviewed_curls(), 0, "the unattended curl was not reviewed");
    let results = tool_results(&llm.requests()[5]);
    assert!(
        results.contains("needs a user attached to this agent"),
        "B reads why: {results}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.root.join("notes.md")).unwrap(),
        "release notes\n",
        "the in-project write ran"
    );
    // A client attaches to B mid-turn: it is shown what B was denied.
    let mut c = rig::Client::attach_agent(
        &ws.root,
        AgentChoice::Agent {
            name: b_name.clone(),
        },
    )
    .await;
    c.wait_for(|m| {
        matches!(m, Msg::Notice { text, .. }
            if text.contains("wanted to run `curl -s http://127.0.0.1:9/notes`"))
    })
    .await;
    gate.notify_one();
    llm.wait_call(6).await;
    assert_eq!(reviewed_curls(), 1, "attached, B's next curl was reviewed");
    let result = tool_result(&llm.requests()[6], "b3");
    assert!(
        !result.contains("needs a user attached"),
        "the reviewed curl ran: {result}"
    );
    // Answered once: B's next client is told nothing more.
    let (again, _notes) = rkat_tui::client::transport::Connection::attach_to(
        &daemon.endpoint(),
        std::time::Duration::from_secs(5),
        None,
        false,
        Some(AgentChoice::Agent { name: b_name }),
    )
    .await
    .unwrap();
    let missed = again.call("rkat/missed", json!({})).await.unwrap();
    assert_eq!(missed, json!({"notices": []}));
    drop(again);
    c.quit().await;
    a.quit().await;
    daemon.stop().await;
}

/// The other half of A6: a turn the user started stays attended after its
/// client detaches (D2: a detached agent keeps running): its `curl` goes to
/// the classifier, not to the unattended denial. This pins that the run a
/// tool call names is the run the daemon saw start after the user's input.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_the_user_started_is_reviewed_after_its_client_detaches() {
    use rkat_tui::ui::{Departure, Effect};
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(
            Arc::clone(&gate),
            Box::new(Step::Tool {
                id: "c1",
                name: "shell",
                args: json!({"command": "curl -s http://127.0.0.1:9/notes"}),
            }),
        ),
        Step::Text("fetched"),
    ]);
    let classifier = rig::ScriptedClassifier::allowing(Vec::new());
    let setup = rig::Setup::default().classifier(Arc::clone(&classifier) as _);
    let args = setup.command_line(&ws.root);
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let client = rig::Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("fetch the notes".into()));
    llm.wait_call(0).await;
    client.send(Effect::Leave(Departure::Detach));
    client.quit().await;
    common::within(async {
        while daemon
            .status()
            .await
            .agents
            .iter()
            .any(|row| row.clients > 0)
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    gate.notify_one();
    llm.wait_call(1).await;
    assert_eq!(classifier.tools(), ["shell"], "the curl was reviewed");
    let result = tool_result(&llm.requests()[1], "c1");
    assert!(!result.contains("needs a user attached"), "{result}");
    daemon.stop().await;
}

/// B quits (retires): A is told it left at its next turn, and a message A
/// sends B with the peer id it had is not delivered; nothing runs for B.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quit_agent_is_announced_and_takes_no_more_messages() {
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::{Effect, Msg};
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
        Step::Text("sentinel answered"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = rig::Client::attach(&ws.root).await;
    let mut b = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    *b_name.lock() = b.agent().await;
    let name = b_name.lock().clone();
    a.send(Effect::StartTurn("who is here".into()));
    assert_eq!(a.turn_finished().await.unwrap().text, "listed");
    b.send(Effect::DecideQuit);
    b.wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.contains("retired")))
        .await;
    b.quit().await;
    a.send(Effect::StartTurn("ask b".into()));
    assert_eq!(a.turn_finished().await.unwrap().text, "tried");
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
    // A sentinel turn on the agent left: its request is the next one the
    // model gets, so a run for the retired agent (which the send would
    // have started at once) would have come first.
    a.send(Effect::StartTurn("sentinel".into()));
    assert_eq!(a.turn_finished().await.unwrap().text, "sentinel answered");
    let seen = llm.recorded();
    assert_eq!(seen.len(), 5, "nothing ran for the retired agent");
    assert!(seen[4].user().contains("sentinel"), "{:?}", seen[4].user());
    a.quit().await;
    daemon.stop().await;
}

/// The finding this guards (second review, item 9): a switch whose
/// target failed to spawn, and whose previous conversation could not be
/// seated again either, left the agent out of the mob but listed as live
/// (a ghost: `/agents`, `--list` and the proxy counted it, and its
/// prompts met engine errors).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_lost_in_a_failed_switch_is_no_ghost() {
    use rkat_tui::client::transport::Connection;
    use rkat_tui::daemon::protocol::{AgentChoice, Status};
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("b answered")]);
    let faults = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let daemon = Running::start_with(
        &common::args_for(&ws.root),
        Running::options(&ws.root, &llm).with_spawn_faults(Arc::clone(&faults)),
    )
    .await;
    let a = daemon.connect_bound().await;
    let (b, _notes) = Connection::attach_to(
        &daemon.endpoint(),
        std::time::Duration::from_secs(5),
        None,
        false,
        Some(AgentChoice::New),
    )
    .await
    .unwrap();
    let lost = daemon.agent.clone();
    assert_ne!(b.agent(), lost);
    // A file watch of the agent about to be lost (review 5, A1: a lost
    // agent kept its watches, unlike a retired one).
    let watches = Arc::clone(daemon.local().agents().watches());
    rkat_tui::tools::WatchHost::watch(&*watches, &lost, &["*.txt".to_string()], false).unwrap();
    assert_eq!(watches.describe(&lost).len(), 1);
    // The new conversation fails to spawn, and so does seating the old one.
    faults.store(2, std::sync::atomic::Ordering::SeqCst);
    let refused = a.call("rkat/new", json!({})).await.unwrap_err().to_string();
    assert!(
        refused.contains(&format!("agent {lost} was lost")),
        "{refused}"
    );
    let status: Status =
        rkat_tui::client::remote::call::<rkat_tui::daemon::protocol::method::Status>(
            &b,
            &rkat_tui::daemon::protocol::Empty {},
        )
        .await
        .unwrap();
    assert!(
        status.agents.iter().all(|row| row.name != lost),
        "no ghost: {:?}",
        status.agents
    );
    assert!(
        watches.describe(&lost).is_empty(),
        "a lost agent's watches go with it"
    );
    drop(watches);
    // The other agent was told it left.
    b.call(
        "mob/turn_start",
        json!({"mob_id": "tui", "agent_identity": b.agent(), "prompt": "who is here"}),
    )
    .await
    .unwrap();
    let told = format!("{:?}", llm.recorded()[0].messages);
    assert!(
        told.contains(&format!("Agent `{lost}` left this workspace")),
        "{told}"
    );
    drop((a, b));
    daemon.stop().await;
}

/// A new agent's attach is answered while another agent's helper runs a
/// turn: seating it waits on nothing of the other agent's work. The proof
/// is that the attach returns at all while the helper's model call is held
/// on a gate that opens only after it: an attach that waited on the helper
/// could not return. How long seating takes is the host's (a loaded CI
/// runner took 6 s), so no time bound is asserted beyond the wait's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_agent_attaches_while_another_agents_helper_runs() {
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::Effect;
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "d1",
            name: "delegate",
            args: json!({
                "member_id": "scout",
                "task": "HELPER-TASK: look around",
                "result_label": "result",
                "max_text_bytes": 4000,
            }),
        },
        Step::Gated(Arc::clone(&gate), Box::new(Step::Text("HELPER-DONE"))),
        Step::Text("parent done"),
        Step::Text("next"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = rig::Client::attach(&ws.root).await;
    a.send(Effect::StartTurn("delegate it".into()));
    // The helper's model call waits on the gate: the helper is running.
    llm.wait_call(1).await;
    let started = std::time::Instant::now();
    let b = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        rig::Client::attach_agent(&ws.root, AgentChoice::New),
    )
    .await;
    let waited = started.elapsed();
    // Only now may the helper finish.
    gate.notify_one();
    let mut b = b.unwrap_or_else(|_| panic!("the attach waited on the helper ({waited:?})"));
    let b_name = b.agent().await;
    assert_ne!(a.agent().await, b_name);
    assert_eq!(a.turn_finished().await.unwrap().text, "parent done");
    // The note that B joined landed after A's turn: A's next turn reads it.
    a.send(Effect::StartTurn("again".into()));
    assert_eq!(a.turn_finished().await.unwrap().text, "next");
    let last = llm.recorded().pop().unwrap();
    assert!(
        format!("{:?}", last.messages).contains(&format!("Agent `{b_name}` joined this workspace")),
        "A was told B joined"
    );
    a.quit().await;
    b.quit().await;
    daemon.stop().await;
}

/// Item 11: in manual mode a message to another agent of the workspace
/// asks nothing (tier 1: the recipient's own calls still pass its gate),
/// and a message to any other peer id is still reviewed, so manual mode
/// asks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_to_another_agent_is_tier_one_and_any_other_send_asks() {
    use rkat_tui::approval::mode::PermissionMode;
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::{Effect, EscalationAnswer};
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
        // A's next call and B's run (woken by the message) race for
        // these two: they are alike.
        Step::Text("ok"),
        Step::Text("ok"),
        // A's second turn: a send to a peer id no agent has.
        Step::Tool {
            id: "m2",
            name: "send_message",
            args: json!({"peer_id": "6f9619ff-8b86-d011-b42d-00cf4fc964ff", "body": "hello", "handling_mode": "queue"}),
        },
        Step::Text("told b"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = rig::Client::attach(&ws.root).await;
    let mut b = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    *b_name.lock() = b.agent().await;
    a.set_mode(PermissionMode::Manual).await;
    a.send(Effect::StartTurn("tell b".into()));
    // The message to B ran without a question: B got it.
    b.wait_for(|m| matches!(m, rkat_tui::ui::Msg::Peer { .. }))
        .await;
    assert_eq!(a.turn_finished().await.unwrap().text, "ok");
    assert_eq!(a.escalations(), 0);
    let _ = b.turn_finished().await;
    // The unknown peer asks.
    a.send(Effect::StartTurn("and someone else".into()));
    let (prompt, id) = a.escalation().await;
    assert_eq!(prompt.tool, "send_message");
    assert!(prompt.args_preview.contains("6f9619ff"), "{prompt:?}");
    a.send(Effect::RespondEscalation(
        id,
        EscalationAnswer::Deny { comment: None },
    ));
    assert_eq!(a.turn_finished().await.unwrap().text, "told b");
    assert_eq!(a.escalations(), 1);
    a.quit().await;
    b.quit().await;
    daemon.stop().await;
}

/// Each agent's question is shown on its own client at once: with agent
/// A's dialog left open on A's client, agent B's call is put to B's client
/// and answered there, while A's still waits. The broker once serialized
/// every escalation of the daemon, so B's question waited unseen behind
/// A's (a client shows its own agent's dialogs only) until its budget ran
/// out as "the user did not answer".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_agents_question_is_shown_on_its_client_while_another_is_open() {
    use rkat_tui::approval::mode::PermissionMode;
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::{Effect, EscalationAnswer};
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "a1",
            name: "shell",
            args: json!({"command": "curl -s http://127.0.0.1:9/a"}),
        },
        Step::Tool {
            id: "b1",
            name: "shell",
            args: json!({"command": "curl -s http://127.0.0.1:9/b"}),
        },
        Step::Text("b done"),
        Step::Text("a done"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = rig::Client::attach(&ws.root).await;
    let mut b = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    a.set_mode(PermissionMode::Manual).await;
    a.send(Effect::StartTurn("fetch a".into()));
    let (shown_a, id_a) = a.escalation().await;
    assert!(shown_a.args_preview.contains("/a"), "{shown_a:?}");
    // A's dialog stays open; B's agent asks next.
    b.send(Effect::StartTurn("fetch b".into()));
    let (shown_b, id_b) = b.escalation().await;
    assert!(shown_b.args_preview.contains("/b"), "{shown_b:?}");
    b.send(Effect::RespondEscalation(
        id_b,
        EscalationAnswer::Deny { comment: None },
    ));
    assert_eq!(b.turn_finished().await.unwrap().text, "b done");
    // A's question was shown on A only, B's on B only, and A's still waits.
    assert_eq!((a.escalations(), b.escalations()), (1, 1));
    a.send(Effect::RespondEscalation(
        id_a,
        EscalationAnswer::Deny { comment: None },
    ));
    assert_eq!(a.turn_finished().await.unwrap().text, "a done");
    a.quit().await;
    b.quit().await;
    daemon.stop().await;
}

/// `rkat/recall` acts on the caller's own agent only: agent B naming agent
/// A's queued steer (and an id that names no input at all) gets both back
/// as foreign, nothing is cancelled, and A's steer reaches A's model. A
/// probe, bound to no agent, is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_cannot_recall_another_agents_input() {
    use meerkat_core::lifecycle::InputId;
    use rkat_tui::client::remote;
    use rkat_tui::client::transport::Connection;
    use rkat_tui::daemon::protocol::{self, AgentChoice, RecallParams, method};
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "slow",
            name: "shell",
            args: json!({ "command": "sleep 2; echo done sleeping", "timeout_secs": 30 }),
        },
        Step::Text("finished"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let a = daemon.connect_bound().await;
    let (b, _notes) = Connection::attach_to(
        &daemon.endpoint(),
        std::time::Duration::from_secs(5),
        None,
        false,
        Some(AgentChoice::New),
    )
    .await
    .unwrap();
    b.call("initialize", json!({})).await.unwrap();
    assert_ne!(b.agent(), a.agent());
    remote::send(&a, "do something for a while", false)
        .await
        .unwrap();
    llm.wait_call(0).await;
    // Queued behind the slow shell call (it joins at the next boundary).
    let steer = remote::send(&a, "steer of agent a", true)
        .await
        .unwrap()
        .expect("the daemon names the steer's input");
    let unknown = InputId::new();
    let params = RecallParams {
        input_ids: vec![steer.clone(), unknown.clone()],
    };
    let answer = remote::call::<method::Recall>(&b, &params).await.unwrap();
    assert!(answer.recalled.is_empty(), "{answer:?}");
    assert!(answer.reached.is_empty(), "{answer:?}");
    assert!(answer.errors.is_empty(), "{answer:?}");
    assert_eq!(answer.foreign, vec![steer, unknown]);
    let probe = daemon.connect().await;
    let refused = remote::call::<method::Recall>(&probe, &params)
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, rkat_tui::client::ClientError::Rpc { code, .. } if *code == protocol::NO_AGENT),
        "{refused:?}"
    );
    llm.wait_call(1).await;
    let steered = common::user_texts(&llm.recorded()[1].messages);
    assert!(
        steered.iter().any(|t| t == "steer of agent a"),
        "the steer stayed and reached its agent: {steered:?}"
    );
    drop((a, b, probe));
    daemon.stop().await;
}

/// What a child's `peers` lists, by route, run as the agent's helpers
/// would (`HELPER-TASK` marks the child's conversation). Returns the
/// child's `peers` result.
async fn child_peers(route: &'static str) -> (String, String) {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        let child = messages
            .iter()
            .any(|m| matches!(m, Message::User(_)) && format!("{m:?}").contains("HELPER-TASK"));
        let results = tool_results(messages);
        if child {
            let asked = messages.iter().any(|m| {
                matches!(m, Message::ToolResults { results, .. } if results.iter().any(|r| r.tool_use_id == "cp1"))
            });
            if asked {
                return Step::Text("child done");
            }
            return Step::Tool {
                id: "cp1",
                name: "peers",
                args: json!({}),
            };
        }
        if !results.is_empty() {
            return Step::Text("parent done");
        }
        match route {
            "delegate" => Step::Tool {
                id: "r1",
                name: "delegate",
                args: json!({
                    "member_id": "kid",
                    "task": "HELPER-TASK list your peers",
                    "result_label": "result",
                    "max_text_bytes": 4000,
                }),
            },
            "spawn_member" => Step::Tool {
                id: "r1",
                name: "spawn_member",
                args: json!({
                    "profile": "main",
                    "member_id": "kid",
                    "initial_message": "HELPER-TASK list your peers",
                }),
            },
            _ => Step::Tool {
                id: "r1",
                name: "fork_off",
                args: json!({"member_id": "kid", "task": "HELPER-TASK list your peers"}),
            },
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 10]);
    let daemon = Running::start(&ws.root, &llm).await;
    if route == "fork_off" {
        daemon.reveal(&daemon.session, &["fork_off"]).await;
    }
    let connection = daemon.connect_bound().await;
    connection
        .call(
            "mob/turn_start",
            json!({"mob_id": "tui", "agent_identity": daemon.agent, "prompt": "start a child"}),
        )
        .await
        .unwrap();
    let peers = common::within(async {
        loop {
            let found = llm.requests().iter().find_map(|messages| {
                messages.iter().find_map(|m| match m {
                    Message::ToolResults { results, .. } => results
                        .iter()
                        .find(|r| r.tool_use_id == "cp1")
                        .map(meerkat_core::types::ToolResult::text_content),
                    _ => None,
                })
            });
            if let Some(found) = found {
                return found;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    let agent = daemon.agent.clone();
    drop(connection);
    daemon.stop().await;
    (peers, agent)
}

/// Every child an agent starts with `delegate` or `spawn_member` is wired
/// to it without being asked: its `peers` names the agent. A `fork_off`
/// child is wired to no one (the engine seats it without a trust edge and
/// asks rkat-tui's customizer only for its gate), so its `peers` is empty;
/// the persona says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn children_are_wired_to_their_parent_except_forks() {
    for route in ["delegate", "spawn_member"] {
        let (peers, agent) = child_peers(route).await;
        assert!(peers.contains(&agent), "{route}: {peers}");
    }
    let (peers, agent) = child_peers("fork_off").await;
    assert!(!peers.contains(&agent), "fork_off: {peers}");
}

/// A parent messages its own child as it messages another agent: the
/// send is tier 1 (no classifier review; the child's own gate judges what
/// it does about it), and the child's next run reads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parent_messages_its_child_like_a_peer() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let text = format!("{messages:?}");
        let child = messages
            .iter()
            .any(|m| matches!(m, Message::User(_)) && format!("{m:?}").contains("HELPER-TASK"));
        if child {
            return Step::Text("child ok");
        }
        let answered = |id: &str| {
            messages.iter().any(|m| {
                matches!(m, Message::ToolResults { results, .. } if results.iter().any(|r| r.tool_use_id == id))
            })
        };
        if !answered("r1") {
            Step::Tool {
                id: "r1",
                name: "spawn_member",
                args: json!({
                    "profile": "main",
                    "member_id": "kid",
                    "initial_message": "HELPER-TASK wait for word",
                }),
            }
        } else if !answered("p1") {
            Step::Tool {
                id: "p1",
                name: "peers",
                args: json!({}),
            }
        } else if !answered("m1") {
            let peer = peer_id_of(messages, "kid").unwrap_or_else(|| "none".into());
            Step::Tool {
                id: "m1",
                name: "send_message",
                args: json!({"peer_id": peer, "body": "PARENT-WORD go on", "handling_mode": "queue"}),
            }
        } else if text.contains("PARENT-WORD") {
            Step::Text("told the kid")
        } else {
            Step::Text("?")
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 12]);
    let classifier = rig::ScriptedClassifier::allowing(Vec::new());
    let setup = rig::Setup::default().classifier(Arc::clone(&classifier) as _);
    let args = setup.command_line(&ws.root);
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let connection = daemon.connect_bound().await;
    connection
        .call(
            "mob/turn_start",
            json!({"mob_id": "tui", "agent_identity": daemon.agent, "prompt": "seat a kid and tell it"}),
        )
        .await
        .unwrap();
    let sent = common::within(async {
        loop {
            let reply = llm.requests().iter().find_map(|messages| {
                messages.iter().find_map(|m| match m {
                    Message::ToolResults { results, .. } => results
                        .iter()
                        .find(|r| r.tool_use_id == "m1")
                        .map(meerkat_core::types::ToolResult::text_content),
                    _ => None,
                })
            });
            if let Some(reply) = reply {
                return reply;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(sent.contains("sent") || sent.contains("accepted"), "{sent}");
    assert!(
        !classifier.tools().iter().any(|tool| tool == "send_message"),
        "{:?}",
        classifier.tools()
    );
    // The kid reads it in a run of its own.
    common::within(async {
        loop {
            let read = llm.requests().iter().any(|messages| {
                messages.iter().any(|m| {
                    matches!(m, Message::User(_)) && format!("{m:?}").contains("HELPER-TASK")
                }) && format!("{messages:?}").contains("PARENT-WORD")
            });
            if read {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    drop(connection);
    daemon.stop().await;
}
