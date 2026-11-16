//! `fork_off` children wired to their parents (the owner's decision of
//! 2026-11-16): the engine seats a fork with no edge, and the daemon wires
//! it to its parent once it is seated (`mobs::follow`); its first calls wait
//! for that edge. The engine tells the parent of the fork's first run's
//! end; every later run that leaves it idle is told by the daemon.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::Running;
use serde_json::json;

use crate::assign::peer_id;

/// The text of the last message of `messages`, whatever its kind.
fn last(messages: &[Message]) -> String {
    messages
        .last()
        .map(|m| format!("{m:?}"))
        .unwrap_or_default()
}

/// Whether the last message is a tool result holding a `peers` listing.
fn after_peers(messages: &[Message]) -> bool {
    matches!(messages.last(), Some(Message::ToolResults { results, .. })
        if results.iter().any(|r| r.text_content().contains("\"peers\"")))
}

/// Whether the last message answers the tool call `id`.
fn answered(messages: &[Message], id: &str) -> bool {
    matches!(messages.last(), Some(Message::ToolResults { results, .. })
        if results.iter().any(|r| r.tool_use_id == id))
}

fn send(id: &'static str, peer: Option<String>, body: &str) -> Step {
    Step::Tool {
        id,
        name: "send_message",
        args: json!({"peer_id": peer.unwrap_or_default(), "body": body, "handling_mode": "queue"}),
    }
}

fn peers(id: &'static str) -> Step {
    Step::Tool {
        id,
        name: "peers",
        args: json!({}),
    }
}

fn fork(task: &str) -> Step {
    Step::Tool {
        id: "f1",
        name: "fork_off",
        args: json!({"member_id": "kid", "task": task}),
    }
}

/// Whether a recorded request of the fork (`fork`: its task names
/// `FORK-TASK`) or of its parent carries `text`.
fn heard(llm: &ScriptedLlmClient, fork: bool, text: &str) -> bool {
    llm.recorded()
        .iter()
        .filter(|r| r.user().contains("FORK-TASK") == fork)
        .any(|r| format!("{:?}", r.messages).contains(text))
}

/// The `wired_to` of `member` in the workspace's mob.
async fn wired_to(
    connection: &rkat_tui::client::transport::Connection,
    member: &str,
) -> Vec<String> {
    let members = connection
        .call("mob/members", json!({"mob_id": "tui"}))
        .await
        .unwrap();
    members["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["agent_identity"] == member)
        .and_then(|m| m["wired_to"].as_array())
        .map(|wired| {
            wired
                .iter()
                .filter_map(|w| w.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A fork and its parent, each claim shown positively:
///
/// - wired from its first call: the fork's first step is `peers`, which
///   lists its parent (its calls waited for the edge);
/// - duplex: the fork's message is read by the parent's model, the parent's
///   by the fork's (in a later run of the fork: its first run had ended);
/// - the parent's message to its fork is a peer message (no review);
/// - told: the engine's outcome tells the first run's end, and nothing of
///   ours does (decided, not told); the run the parent's message started is
///   told as "went idle" with its last message, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_and_its_parent_talk_both_ways_and_its_later_runs_are_told() {
    let ws = Workspace::new();
    let reply = Arc::new(tokio::sync::Notify::new());
    let step = {
        let reply = Arc::clone(&reply);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let tail = last(messages);
            if common::user_texts(messages).concat().contains("FORK-TASK") {
                if tail.contains("PARENT-HELLO") {
                    return Step::Text("FORK-LATER");
                }
                if after_peers(messages) {
                    return send("fm1", peer_id(messages, "main-"), "FORK-HELLO");
                }
                if answered(messages, "fm1") {
                    return Step::Text("FORK-FIRST");
                }
                return peers("fp1");
            }
            // The parent's steps are keyed on its whole conversation: the
            // fork's message and the engine's outcome may come in one run.
            if tail.contains("went idle") {
                return Step::Text("NOTED");
            }
            let all = format!("{messages:?}");
            if all.contains("\"pp1\"") && !all.contains("\"pm1\"") {
                return Step::Gated(
                    Arc::clone(&reply),
                    Box::new(send("pm1", peer_id(messages, "kid"), "PARENT-HELLO")),
                );
            }
            if all.contains("FORK-HELLO") && !all.contains("\"pp1\"") {
                return peers("pp1");
            }
            if all.contains("\"f1\"") {
                return Step::Text("PARENT-WAITS");
            }
            fork("FORK-TASK: talk to your parent")
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 30]);
    let classifier = rig::ScriptedClassifier::allowing(Vec::new());
    let setup = rig::Setup::default().classifier(Arc::clone(&classifier) as _);
    let args = setup.command_line(&ws.root);
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    daemon.reveal(&daemon.session, &["fork_off"]).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    // Its first call already found its parent.
    common::until(|| heard(&llm, true, "\"peers\"")).await;
    let first = llm
        .recorded()
        .iter()
        .filter(|r| r.user().contains("FORK-TASK"))
        .find(|r| after_peers(&r.messages))
        .map(|r| last(&r.messages))
        .unwrap();
    assert!(
        first.contains(&format!("tui/main/{}", daemon.agent)),
        "{first}"
    );
    // The parent reads the fork's message.
    common::until(|| heard(&llm, false, "FORK-HELLO")).await;
    // The parent answers only once the fork's first run was decided (its
    // end is the engine's to tell).
    let registry = daemon.registry();
    common::until(|| registry.endings().idle_decided() >= 1).await;
    assert_eq!(
        registry.endings().told(),
        0,
        "the first run is not ours to tell"
    );
    reply.notify_one();
    // The fork reads it, in a run after its first.
    common::until(|| heard(&llm, true, "PARENT-HELLO")).await;
    let later = llm
        .recorded()
        .iter()
        .filter(|r| r.user().contains("FORK-TASK"))
        .map(|r| format!("{:?}", r.messages))
        .find(|m| m.contains("PARENT-HELLO"))
        .unwrap();
    assert!(later.contains("FORK-FIRST"), "{later}");
    // Told of that run, with its last message, once.
    common::until(|| heard(&llm, false, "Your helper `kid` went idle")).await;
    let note = llm
        .recorded()
        .iter()
        .filter(|r| !r.user().contains("FORK-TASK"))
        .map(|r| format!("{:?}", r.messages))
        .find(|m| m.contains("Your helper `kid` went idle"))
        .unwrap();
    assert!(
        note.contains("FORK-LATER") && !note.contains("Its last message:\\n\\nFORK-FIRST"),
        "{note}"
    );
    assert!(heard(&llm, false, "Background fork_off job"));
    assert_eq!(registry.endings().told(), 1);
    // The parent's message to its own fork is a peer message: not reviewed.
    let reviewed = classifier.seen();
    assert!(
        !reviewed
            .iter()
            .any(|(tool, args, _)| tool == "send_message"
                && args.to_string().contains("PARENT-HELLO")),
        "{reviewed:?}"
    );
    // The note's run (heard above) ends before the stop
    // (lukacf/meerkat#2374).
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}

/// The wire comes before the fork is seated (on a slow machine its spawn
/// is still building when the follower hears of it; here the wire is made
/// as the spawn is announced, `HelperRegistry::wire_forks_early`): a
/// "not seated" is tried again until it is, the fork ends up wired, and
/// its first call, which waited for the edge, lists its parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_wired_before_it_is_seated_is_wired_once_it_is() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if common::user_texts(messages).concat().contains("FORK-TASK") {
            if after_peers(messages) {
                return Step::Text("FORK-DONE");
            }
            return peers("fp1");
        }
        if tail_is_outcome(messages) {
            return Step::Text("NOTED");
        }
        if matches!(messages.last(), Some(Message::ToolResults { .. })) {
            return Step::Text("PARENT-WAITS");
        }
        fork("FORK-TASK: list peers")
    }));
    let llm = ScriptedLlmClient::new(vec![step; 12]);
    let daemon = Running::start(&ws.root, &llm).await;
    let registry = daemon.registry();
    registry.wire_forks_early();
    daemon.reveal(&daemon.session, &["fork_off"]).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let agent = daemon.agent.clone();
    common::until_async(async || wired_to(&connection, "kid").await.contains(&agent)).await;
    assert!(
        registry.forks_not_seated() >= 1,
        "the wire came before the fork was seated"
    );
    common::until(|| heard(&llm, true, "\"peers\"")).await;
    let first = llm
        .recorded()
        .iter()
        .filter(|r| r.user().contains("FORK-TASK"))
        .find(|r| after_peers(&r.messages))
        .map(|r| last(&r.messages))
        .unwrap();
    assert!(first.contains(&format!("tui/main/{agent}")), "{first}");
    common::until(|| heard(&llm, false, "Background fork_off job")).await;
    daemon.idle(&agent).await;
    drop(connection);
    daemon.stop().await;
}

/// Whether the last message is the engine's outcome of a fork's first run.
fn tail_is_outcome(messages: &[Message]) -> bool {
    last(messages).contains("Background fork_off job")
}

/// A helper's fork (helpers may nest: `helpers.max_depth = 2`) is seated
/// in the helper's own mob, the workspace's, and wired to the helper that
/// forked it, not to the agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_s_fork_is_wired_to_the_helper() {
    let ws = Workspace::new();
    let go = Arc::new(tokio::sync::Notify::new());
    let step = {
        let go = Arc::clone(&go);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let user = common::user_texts(messages).concat();
            if user.contains("FORK-TASK") {
                if after_peers(messages) {
                    return Step::Text("FORK-DONE");
                }
                return peers("fp1");
            }
            if user.contains("HELPER-TASK") {
                if answered(messages, "f1") {
                    return Step::Hang;
                }
                return Step::Gated(Arc::clone(&go), Box::new(fork("FORK-TASK: list peers")));
            }
            if matches!(messages.last(), Some(Message::ToolResults { .. })) {
                return Step::Text("assigned");
            }
            Step::Tool {
                id: "a1",
                name: "assign",
                args: json!({"name": "scout", "task": "HELPER-TASK: fork"}),
            }
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let setup = rig::Setup::args(&["--set", "helpers.max_depth=2"]);
    let args = setup.command_line(&ws.root);
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let registry = daemon.registry();
    let scout = rkat_tui::mobs::HelperKey::new(
        meerkat_mob::MobId::from("tui"),
        meerkat_mob::AgentIdentity::from("scout"),
    );
    common::until(|| registry.member_session(&scout).is_some()).await;
    let session = registry.member_session(&scout).unwrap();
    daemon.reveal(&session, &["fork_off"]).await;
    go.notify_one();
    common::until(|| heard(&llm, true, "\"peers\"")).await;
    let listed = llm
        .recorded()
        .iter()
        .filter(|r| r.user().contains("FORK-TASK"))
        .find(|r| after_peers(&r.messages))
        .map(|r| last(&r.messages))
        .unwrap();
    assert!(listed.contains("tui/main/scout"), "{listed}");
    assert!(
        !listed.contains(&format!("tui/main/{}", daemon.agent)),
        "{listed}"
    );
    let wired = wired_to(&connection, "kid").await;
    assert_eq!(wired, vec!["scout".to_string()]);
    drop(connection);
    daemon.stop().await;
}

/// The edge's life: a fork restored by the next start is wired to its
/// parent again (the roster names the edge, and the parent's message
/// reaches the fork's model); a fork retired leaves its parent's edges.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_fork_is_wired_and_a_retired_one_is_not() {
    let ws = Workspace::new();
    let first = ScriptedLlmClient::new(vec![
        Step::Dynamic(Arc::new(|messages: &[Message]| {
            let tail = last(messages);
            if common::user_texts(messages).concat().contains("FORK-TASK") {
                return Step::Text("FORK-FIRST");
            }
            if tail.contains("Background fork_off job") {
                return Step::Text("OUTCOME-NOTED");
            }
            if matches!(messages.last(), Some(Message::ToolResults { .. })) {
                return Step::Text("PARENT-WAITS");
            }
            fork("FORK-TASK: wait")
        }));
        12
    ]);
    let daemon = Running::start(&ws.root, &first).await;
    daemon.reveal(&daemon.session, &["fork_off"]).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let agent = daemon.agent.clone();
    common::until_async(async || wired_to(&connection, "kid").await.contains(&agent)).await;
    // The outcome's run ends before the stop (lukacf/meerkat#2374).
    common::until(|| heard(&first, false, "Background fork_off job")).await;
    daemon.idle(&agent).await;
    drop(connection);
    daemon.stop().await;

    let second = ScriptedLlmClient::new(vec![
        Step::Dynamic(Arc::new(|messages: &[Message]| {
            let tail = last(messages);
            if common::user_texts(messages).concat().contains("FORK-TASK") {
                return Step::Text(if tail.contains("AFTER-RESTART") {
                    "FORK-READ-IT"
                } else {
                    "FORK-OTHER"
                });
            }
            if after_peers(messages) {
                return send("pm1", peer_id(messages, "kid"), "AFTER-RESTART");
            }
            if answered(messages, "pm1") {
                return Step::Text("SENT");
            }
            if tail.contains("went idle")
                || matches!(messages.last(), Some(Message::ToolResults { .. }))
            {
                return Step::Text("NOTED");
            }
            peers("pp1")
        }));
        12
    ]);
    let daemon = Running::start(&ws.root, &second).await;
    let connection = daemon.connect_bound().await;
    assert_eq!(daemon.agent, agent);
    common::until_async(async || wired_to(&connection, "kid").await.contains(&agent)).await;
    daemon.prompt(&connection, "tell the kid").await;
    common::until(|| heard(&second, true, "AFTER-RESTART")).await;
    common::until(|| heard(&second, false, "Your helper `kid` went idle")).await;
    daemon.idle(&agent).await;
    // Retired: the parent's edges no longer name it.
    let handle = daemon
        .state()
        .handle_for(&meerkat_mob::MobId::from("tui"))
        .await
        .unwrap();
    handle
        .retire(meerkat_mob::AgentIdentity::from("kid"))
        .await
        .unwrap();
    common::until_async(async || {
        !wired_to(&connection, &agent)
            .await
            .contains(&"kid".to_string())
    })
    .await;
    drop(connection);
    daemon.stop().await;
}
