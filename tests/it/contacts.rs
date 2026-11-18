//! `contacts`: the one tool that tells an agent or a helper who it can
//! message and what each is. Its entries are exactly the peers the
//! engine lets the caller send to (its comms runtime's directory), each
//! sorted by the daemon's own state.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, is_helper, start};
use meerkat_core::Message;
use rig::{Client, Running, Setup};
use rkat_tui::client::remote;
use rkat_tui::daemon::protocol::{WireCard, WireParams, method};
use serde_json::{Value, json};

fn tool(id: &'static str, name: &'static str, args: Value) -> Step {
    Step::Tool { id, name, args }
}

/// The text of the last user message of `messages`.
fn last_user(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User(user) => Some(user.text_content()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Whether `messages` answer the call `id`.
fn answered(messages: &[Message], id: &str) -> bool {
    messages.iter().any(|m| {
        matches!(m, Message::ToolResults { results, .. } if results.iter().any(|r| r.tool_use_id == id))
    })
}

/// Whether `messages` are the kept fork's (its task names `FK-TASK`).
fn is_fork(messages: &[Message]) -> bool {
    common::user_texts(messages)
        .iter()
        .any(|text| text.contains("FK-TASK") && text.contains("DELEGATED DURABLE FORK"))
}

/// The script. The agent starts `kf` (fresh, kept), `ks` (fresh,
/// single-use, held in its run) and `fk` (a kept fork) on `PHASE-START`,
/// connects `kf` and `ks` on `PHASE-CONNECT`, lists its contacts on
/// `LIST<n>` (call `l<n>`) and retires `kf` on `PHASE-RETIRE`. `ks`, once
/// `go` opens, lists its own (`ksc`) and then waits; it waits again after
/// a restart.
fn script(go: &Arc<tokio::sync::Notify>) -> Arc<ScriptedLlmClient> {
    let go = Arc::clone(go);
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if is_fork(messages) {
            return Step::Text("FK-DONE");
        }
        if is_helper(messages, "KF-TASK") {
            return Step::Text("KF-DONE");
        }
        if is_helper(messages, "KS-TASK") {
            if answered(messages, "ksc") {
                return Step::Hang;
            }
            return Step::Gated(
                Arc::clone(&go),
                Box::new(tool("ksc", "contacts", json!({}))),
            );
        }
        if matches!(messages.last(), Some(Message::ToolResults { .. })) {
            return Step::Text("ok");
        }
        let user = last_user(messages);
        if user.contains("PHASE-START") {
            return Step::Tools(vec![
                (
                    "t-kf",
                    "assign",
                    json!({"name": "kf", "task": "KF-TASK: index the parser\nand more", "keep": true}),
                ),
                (
                    "t-ks",
                    "assign",
                    json!({"name": "ks", "task": "KS-TASK: run the slow suite"}),
                ),
            ]);
        }
        if user.contains("PHASE-FORK") {
            return start("t-fk", "fk", "FK-TASK: think it over", "fork", true);
        }
        if user.contains("PHASE-CONNECT") {
            return tool("t-c", "connect_helpers", json!({"helpers": ["kf", "ks"]}));
        }
        if user.contains("PHASE-RETIRE") {
            return tool("t-r", "retire_helper", json!({"name": "kf"}));
        }
        for (marker, id) in [("LIST1", "l1"), ("LIST2", "l2"), ("LIST3", "l3")] {
            if user.contains(marker) {
                return tool(id, "contacts", json!({}));
            }
        }
        Step::Text("noted")
    }));
    ScriptedLlmClient::new(vec![step; 200])
}

/// The result of the call `id`, parsed.
fn listed(llm: &ScriptedLlmClient, id: &str) -> Option<Value> {
    llm.recorded().iter().find_map(|r| {
        r.messages.iter().find_map(|m| match m {
            Message::ToolResults { results, .. } => results
                .iter()
                .find(|r| r.tool_use_id == id)
                .and_then(|r| serde_json::from_str(&r.text_content()).ok()),
            _ => None,
        })
    })
}

/// Every peer id `value` names.
fn ids(value: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    fn walk(value: &Value, out: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                if let Some(id) = map.get("peer_id").and_then(Value::as_str) {
                    out.insert(id.to_string());
                }
                map.values().for_each(|v| walk(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(value, &mut out);
    out
}

/// The peer ids the engine lets `session` send to.
async fn sendable(daemon: &Running, session: &meerkat_core::SessionId) -> BTreeSet<String> {
    let comms = daemon
        .state()
        .session_service()
        .comms_runtime(session)
        .await
        .expect("a comms runtime");
    comms
        .peers()
        .await
        .into_iter()
        .map(|peer| peer.peer_id.to_string())
        .collect()
}

/// The names of a group's entries.
fn names(value: &Value, group: &str) -> Vec<String> {
    value[group]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The entry `name` of `group`.
fn entry<'a>(value: &'a Value, group: &str, name: &str) -> &'a Value {
    value[group]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["name"] == name))
        .unwrap_or_else(|| panic!("no {name} in {group}: {value}"))
}

/// Prompt the agent and wait for its run to end.
async fn turn(daemon: &Running, connection: &rkat_tui::client::transport::Connection, text: &str) {
    daemon.prompt(connection, text).await;
    daemon.idle(&daemon.agent).await;
}

/// An agent with a kept and a single-use fresh helper, connected, and a
/// kept fork; a second agent of the workspace with a client; a wire to an
/// agent of another workspace. Its `contacts` names each in its group
/// with its kind, state, task line and connection, and its peer ids are
/// exactly the ones the engine lets it send to; a helper's names its
/// parent and the helper it was connected with, likewise. After a retire
/// the helper is gone and counted ended; after a restart the held ones
/// are listed again, still equal to the engine's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contacts_lists_exactly_the_sendable_peers_by_what_each_is() {
    use rkat_tui::daemon::protocol::AgentChoice;
    let ws = Workspace::new();
    let go = Arc::new(tokio::sync::Notify::new());
    let llm = script(&go);
    let setup = Setup::args(&["--set", "helpers.max_depth=2"]);
    let args = setup.command_line(&ws.root);
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let connection = daemon.connect_bound().await;
    let session = daemon.current_session().await;
    turn(&daemon, &connection, "PHASE-START").await;
    turn(&daemon, &connection, "PHASE-FORK").await;
    let registry = daemon.registry();
    let key = |name: &str| {
        rkat_tui::mobs::HelperKey::new(
            meerkat_mob::MobId::from("tui"),
            meerkat_mob::AgentIdentity::from(name),
        )
    };
    // kf and fk idle, ks waiting in its run.
    common::until(|| {
        registry
            .activity(&key("kf"))
            .is_some_and(|(running, _)| !running)
            && registry
                .activity(&key("fk"))
                .is_some_and(|(running, _)| !running)
            && registry
                .activity(&key("ks"))
                .is_some_and(|(running, _)| running)
    })
    .await;
    daemon.idle(&daemon.agent).await;
    turn(&daemon, &connection, "PHASE-CONNECT").await;
    // A second agent, with a client; a wire to another workspace's.
    let mut other = Client::attach_agent(&ws.root, AgentChoice::New).await;
    let other_name = other.agent().await;
    let gamma = ws.sibling("gamma");
    std::fs::create_dir_all(&gamma).unwrap();
    let key_pair = meerkat_comms::Keypair::generate();
    let public = key_pair.public_key();
    let card = WireCard {
        workspace: gamma.clone(),
        agent: "main-gggg".into(),
        peer_id: public.to_peer_id().to_string(),
        pubkey: public.to_pubkey_string(),
        address: format!("uds://{}", gamma.join("nothing.sock").display()),
    };
    remote::call::<method::Wire>(
        &connection,
        &WireParams {
            agent: None,
            card: card.clone(),
        },
    )
    .await
    .unwrap();
    turn(&daemon, &connection, "LIST1").await;
    let first = common::poll(async || listed(&llm, "l1")).await;
    assert_eq!(
        first["you"],
        format!("{} (an agent of this workspace)", daemon.agent)
    );
    assert_eq!(names(&first, "your_helpers"), ["fk", "kf", "ks"], "{first}");
    let kf = entry(&first, "your_helpers", "kf");
    assert_eq!(kf["kind"], "fresh · kept", "{first}");
    assert!(
        kf["state"].as_str().unwrap().starts_with("idle for "),
        "{first}"
    );
    assert_eq!(kf["task"], "KF-TASK: index the parser", "{first}");
    assert_eq!(kf["connected_to"], json!(["ks"]), "{first}");
    let ks = entry(&first, "your_helpers", "ks");
    assert_eq!(ks["kind"], "fresh · single-use", "{first}");
    assert_eq!(ks["state"], "running", "{first}");
    assert_eq!(ks["connected_to"], json!(["kf"]), "{first}");
    assert_eq!(
        entry(&first, "your_helpers", "fk")["kind"],
        "fork · kept",
        "{first}"
    );
    assert_eq!(
        first["helpers_note"],
        "0 of your helpers have ended in this conversation"
    );
    let agent = entry(&first, "agents_of_this_workspace", &other_name);
    assert_eq!(agent["what"], "idle, a client attached", "{first}");
    let wired = entry(&first, "wired_agents", "gamma/main-gggg");
    assert_eq!(wired["peer_id"], card.peer_id, "{first}");
    assert_eq!(ids(&first), sendable(&daemon, &session).await, "{first}");

    // The single-use helper's own: its parent, the helper it was
    // connected with; no helpers of its own (it may start them).
    go.notify_one();
    let mine = common::poll(async || listed(&llm, "ksc")).await;
    assert_eq!(mine["you"], "ks (a helper, fresh · single-use)", "{mine}");
    assert_eq!(mine["parent"]["name"], daemon.agent.as_str(), "{mine}");
    assert_eq!(names(&mine, "connected_with"), ["kf"], "{mine}");
    assert_eq!(
        entry(&mine, "connected_with", "kf")["what"],
        format!("{}'s helper", daemon.agent),
        "{mine}"
    );
    assert_eq!(mine["helpers_note"], "you hold no helpers now", "{mine}");
    let ks_session = registry.member_session(&key("ks")).unwrap();
    assert_eq!(ids(&mine), sendable(&daemon, &ks_session).await, "{mine}");

    // kf retired: gone, and counted.
    turn(&daemon, &connection, "PHASE-RETIRE").await;
    common::until(|| registry.member_session(&key("kf")).is_none()).await;
    turn(&daemon, &connection, "LIST2").await;
    let second = common::poll(async || listed(&llm, "l2")).await;
    assert_eq!(names(&second, "your_helpers"), ["fk", "ks"], "{second}");
    assert_eq!(
        second["helpers_note"],
        "1 of your helpers have ended in this conversation"
    );
    assert_eq!(ids(&second), sendable(&daemon, &session).await, "{second}");

    // A restart: the held ones are listed again, as the engine has them.
    other.quit().await;
    drop(connection);
    daemon.stop().await;
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let connection = daemon.connect_bound().await;
    let session = daemon.current_session().await;
    turn(&daemon, &connection, "LIST3").await;
    let third = common::poll(async || listed(&llm, "l3")).await;
    let mut held = names(&third, "your_helpers");
    held.sort();
    assert_eq!(held, ["fk", "ks"], "{third}");
    assert_eq!(ids(&third), sendable(&daemon, &session).await, "{third}");
    drop(connection);
    daemon.stop().await;
}
