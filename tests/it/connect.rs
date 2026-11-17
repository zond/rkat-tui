//! `connect_helpers`: an agent wires helpers it started to each other.
//! Each claim is shown positively: each helper's next model request holds
//! the other's message, each was told whom it can reach, the messages
//! between them passed the gate without review (the classifier saw
//! nothing), the edges outlive a restart, and a retired end is gone from
//! its peer's `peers`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::{Running, ScriptedClassifier, Setup};
use serde_json::json;

use crate::assign::peer_id;

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

/// The text of the result of the tool call `id` in `messages`.
fn result_of(messages: &[Message], id: &str) -> Option<(String, bool)> {
    messages.iter().find_map(|m| match m {
        Message::ToolResults { results, .. } => results
            .iter()
            .find(|r| r.tool_use_id == id)
            .map(|r| (r.text_content(), r.is_error)),
        _ => None,
    })
}

fn tool(id: &'static str, name: &'static str, args: serde_json::Value) -> Step {
    Step::Tool { id, name, args }
}

fn send(id: &'static str, peer: Option<String>, body: &str) -> Step {
    tool(
        id,
        "send_message",
        json!({"peer_id": peer.unwrap_or_default(), "body": body, "handling_mode": "queue"}),
    )
}

fn connect(id: &'static str, names: &[&str]) -> Step {
    tool(id, "connect_helpers", json!({ "helpers": names }))
}

/// Whether a recorded request whose user text names `marker` holds `text`.
fn heard(llm: &ScriptedLlmClient, marker: &str, text: &str) -> bool {
    llm.recorded()
        .iter()
        .filter(|r| r.user().contains(marker))
        .any(|r| format!("{:?}", r.messages).contains(text))
}

/// A helper's side of the duplex test: once let go (`go`), it lists its
/// peers, sends `body` to `peer`, and ends its first run only once `end`
/// is notified; a run with the other's `other` text answers `heard`.
#[allow(clippy::too_many_arguments)]
fn talker(
    messages: &[Message],
    ids: (&'static str, &'static str),
    peer: &str,
    body: &str,
    other: &str,
    heard: &'static str,
    go: &Arc<tokio::sync::Notify>,
    end: &Arc<tokio::sync::Notify>,
) -> Step {
    let all = format!("{messages:?}");
    if all.contains(other) {
        return Step::Text(heard);
    }
    if answered(messages, ids.1) {
        return Step::Gated(Arc::clone(end), Box::new(Step::Text("waiting")));
    }
    if after_peers(messages) {
        return send(ids.1, peer_id(messages, peer), body);
    }
    Step::Gated(Arc::clone(go), Box::new(tool(ids.0, "peers", json!({}))))
}

/// No message was put to the classifier: each passed the gate unreviewed.
fn no_send_reviewed(classifier: &ScriptedClassifier) {
    let seen = classifier.seen();
    assert!(
        !seen.iter().any(|(tool, _, _)| tool == "send_message"),
        "{seen:?}"
    );
}

/// A daemon whose classifier records every call it reviews (and allows
/// it): a call it never saw passed without review.
async fn strict(
    ws: &Workspace,
    llm: &Arc<ScriptedLlmClient>,
) -> (Running, Arc<ScriptedClassifier>) {
    let classifier = ScriptedClassifier::allowing(Vec::new());
    let setup = Setup::default().classifier(Arc::clone(&classifier) as _);
    let args = setup.command_line(&ws.root);
    (
        Running::start_with(&args, setup.options(&ws.root, llm)).await,
        classifier,
    )
}

/// Two `assign` helpers, connected: each sends the other a message and
/// each one's next model request holds the other's text (duplex); each
/// was told of the other in a note; the call answered which pairs are new;
/// the messages needed no review.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_assigned_helpers_connected_talk_both_ways() {
    let ws = Workspace::new();
    let gates: Vec<Arc<tokio::sync::Notify>> = (0..4)
        .map(|_| Arc::new(tokio::sync::Notify::new()))
        .collect();
    let step = {
        let gates = gates.clone();
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let user = common::user_texts(messages).concat();
            if user.contains("TASK-OF-A") {
                return talker(
                    messages,
                    ("pa", "ma"),
                    "main/b",
                    "FROM-A-TO-B",
                    "FROM-B-TO-A",
                    "A-HEARD-B",
                    &gates[0],
                    &gates[1],
                );
            }
            if user.contains("TASK-OF-B") {
                return talker(
                    messages,
                    ("pb", "mb"),
                    "main/a",
                    "FROM-B-TO-A",
                    "FROM-A-TO-B",
                    "B-HEARD-A",
                    &gates[2],
                    &gates[3],
                );
            }
            let all = format!("{messages:?}");
            if all.contains("\"c1\"") {
                return Step::Text(if answered(messages, "c1") {
                    "PARENT-DONE"
                } else {
                    "NOTED"
                });
            }
            if answered(messages, "a1") || answered(messages, "a2") {
                return connect("c1", &["a", "b"]);
            }
            Step::Tools(vec![
                (
                    "a1",
                    "assign",
                    json!({"name": "a", "task": "TASK-OF-A: talk to b"}),
                ),
                (
                    "a2",
                    "assign",
                    json!({"name": "b", "task": "TASK-OF-B: talk to a"}),
                ),
            ])
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 40]);
    let (daemon, classifier) = strict(&ws, &llm).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    // The call answered with the pair, new.
    let (answer, error) = llm
        .recorded()
        .iter()
        .find_map(|r| result_of(&r.messages, "c1"))
        .unwrap();
    assert!(!error, "{answer}");
    assert!(answer.contains("`a` and `b` (new)"), "{answer}");
    assert!(answer.contains("`a` is `tui/main/a`"), "{answer}");
    // Both let go only now: their peers lists show each other.
    gates[0].notify_one();
    gates[2].notify_one();
    common::until(|| heard(&llm, "TASK-OF-A", "\"ma\"") && heard(&llm, "TASK-OF-B", "\"mb\""))
        .await;
    gates[1].notify_one();
    gates[3].notify_one();
    // Duplex: each one's next request holds the other's message.
    common::until(|| heard(&llm, "TASK-OF-A", "FROM-B-TO-A")).await;
    common::until(|| heard(&llm, "TASK-OF-B", "FROM-A-TO-B")).await;
    // Each was told of the other, in words it can act on (the note is
    // read at a step's end: by the request after its message at latest).
    for (marker, other) in [
        ("TASK-OF-A", "`b` (`tui/main/b`"),
        ("TASK-OF-B", "`a` (`tui/main/a`"),
    ] {
        assert!(
            heard(
                &llm,
                marker,
                "who started you, connected you with its other helper"
            ) && heard(&llm, marker, other),
            "{marker}"
        );
    }
    common::until(|| {
        llm.recorded()
            .iter()
            .filter(|r| !r.user().contains("TASK-OF-"))
            .any(|r| format!("{:?}", r.messages).contains("Your helper `b` finished"))
            && llm
                .recorded()
                .iter()
                .filter(|r| !r.user().contains("TASK-OF-"))
                .any(|r| format!("{:?}", r.messages).contains("Your helper `a` finished"))
    })
    .await;
    daemon.idle(&daemon.agent).await;
    no_send_reviewed(&classifier);
    drop(connection);
    daemon.stop().await;
}

/// Every refusal says what to do: fewer than two, an unknown name, the
/// caller itself, an agent of the workspace; nothing is connected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_of_what_is_not_the_caller_s_own_helpers_is_refused() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let user = common::user_texts(messages).concat();
        if user.contains("TASK-OF-") {
            return Step::Text("helper idle");
        }
        let all = format!("{messages:?}");
        if all.contains("\"r4\"") {
            return Step::Text("done");
        }
        if all.contains("\"a1\"") {
            // The agent's own name is in its system prompt.
            let me = all
                .split("You are agent `")
                .nth(1)
                .and_then(|rest| rest.split('`').next())
                .unwrap_or("main-xxxx")
                .to_string();
            return Step::Tools(vec![
                ("r1", "connect_helpers", json!({"helpers": ["a"]})),
                ("r2", "connect_helpers", json!({"helpers": ["a", "nobody"]})),
                ("r3", "connect_helpers", json!({"helpers": ["a", me]})),
                (
                    "r4",
                    "connect_helpers",
                    json!({"helpers": ["a", "main-zz99"]}),
                ),
            ]);
        }
        tool(
            "a1",
            "spawn_member",
            json!({"profile": "main", "member_id": "a", "initial_message": "TASK-OF-A", "auto_wire_parent": true}),
        )
    }));
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let (daemon, _classifier) = strict(&ws, &llm).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let results: Vec<(String, bool)> = ["r1", "r2", "r3", "r4"]
        .iter()
        .map(|id| {
            llm.recorded()
                .iter()
                .find_map(|r| result_of(&r.messages, id))
                .unwrap()
        })
        .collect();
    for (text, error) in &results {
        assert!(*error, "{text}");
        assert!(text.contains("your running helpers are `a`"), "{text}");
    }
    assert!(
        results[0].0.contains("name two or more"),
        "{}",
        results[0].0
    );
    assert!(
        results[1]
            .0
            .contains("`nobody` is no running helper of yours"),
        "{}",
        results[1].0
    );
    assert!(results[2].0.contains("is you"), "{}", results[2].0);
    assert!(
        results[3].0.contains("an agent of this workspace"),
        "{}",
        results[3].0
    );
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}

/// A helper of the agent's own mob and a member of a mob it created are
/// in different mobs: refused, saying why and what works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn helpers_in_different_mobs_are_refused_saying_what_works() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let user = common::user_texts(messages).concat();
        if user.contains("TASK-OF-") || !user.contains("go") {
            return Step::Text("idle");
        }
        let all = format!("{messages:?}");
        if all.contains("\"c1\"") {
            return Step::Text("done");
        }
        if all.contains("\"s2\"") {
            return connect("c1", &["a", "w1"]);
        }
        if all.contains("\"s1\"") {
            return tool(
                "s2",
                "mob_spawn_member",
                json!({"mob_id": "team", "profile": "worker", "member_id": "w1"}),
            );
        }
        if all.contains("\"m1\"") {
            return tool(
                "s1",
                "spawn_member",
                json!({"profile": "main", "member_id": "a", "initial_message": "TASK-OF-A", "auto_wire_parent": true}),
            );
        }
        tool(
            "m1",
            "mob_create",
            json!({"definition": {"id": "team", "profiles": {"worker": {"model": common::MODEL, "tools": {"comms": true}}}}}),
        )
    }));
    let llm = ScriptedLlmClient::new(vec![step; 30]);
    let (daemon, _classifier) = strict(&ws, &llm).await;
    daemon
        .reveal(&daemon.session, &["mob_create", "mob_spawn_member"])
        .await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let (text, error) = llm
        .recorded()
        .iter()
        .find_map(|r| result_of(&r.messages, "c1"))
        .unwrap();
    assert!(error, "{text}");
    assert!(text.contains("in different mobs"), "{text}");
    assert!(
        text.contains("`a` in the mob `tui`") && text.contains("`w1` in the mob `team`"),
        "{text}"
    );
    assert!(
        text.contains("Start the helpers that must talk to each other in the same mob"),
        "{text}"
    );
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}

/// Mixed kinds: a fork and a `spawn_member` helper connected talk both
/// ways, without review.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_and_a_kept_member_connected_talk_both_ways() {
    let ws = Workspace::new();
    let gates: Vec<Arc<tokio::sync::Notify>> = (0..4)
        .map(|_| Arc::new(tokio::sync::Notify::new()))
        .collect();
    let step = {
        let gates = gates.clone();
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let user = common::user_texts(messages).concat();
            // The fork's conversation is a copy of its parent's and then
            // its task (the parent's has the task only in its call).
            if user.contains("TASK-OF-KID") {
                return talker(
                    messages,
                    ("pk", "mk"),
                    "main/w",
                    "FROM-KID-TO-W",
                    "FROM-W-TO-KID",
                    "KID-HEARD-W",
                    &gates[0],
                    &gates[1],
                );
            }
            if user.contains("TASK-OF-W") {
                return talker(
                    messages,
                    ("pw", "mw"),
                    "main/kid",
                    "FROM-W-TO-KID",
                    "FROM-KID-TO-W",
                    "W-HEARD-KID",
                    &gates[2],
                    &gates[3],
                );
            }
            let all = format!("{messages:?}");
            if all.contains("\"c1\"") {
                return Step::Text("NOTED");
            }
            if all.contains("\"f1\"") && all.contains("\"s1\"") {
                return connect("c1", &["kid", "w"]);
            }
            Step::Tools(vec![
                (
                    "s1",
                    "spawn_member",
                    json!({"profile": "main", "member_id": "w", "initial_message": "TASK-OF-W", "auto_wire_parent": true}),
                ),
                (
                    "f1",
                    "fork_off",
                    json!({"member_id": "kid", "task": "TASK-OF-KID"}),
                ),
            ])
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 40]);
    let (daemon, classifier) = strict(&ws, &llm).await;
    daemon.reveal(&daemon.session, &["fork_off"]).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let (answer, error) = llm
        .recorded()
        .iter()
        .find_map(|r| result_of(&r.messages, "c1"))
        .unwrap();
    assert!(!error, "{answer}");
    assert!(answer.contains("`kid` and `w` (new)"), "{answer}");
    for gate in &gates {
        gate.notify_one();
    }
    common::until(|| heard(&llm, "TASK-OF-KID", "FROM-W-TO-KID")).await;
    common::until(|| heard(&llm, "TASK-OF-W", "FROM-KID-TO-W")).await;
    // The fork's result run ends before the daemon stops
    // (lukacf/meerkat#2374, #2384).
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| format!("{:?}", r.messages).contains("KID-HEARD-W"))
    })
    .await;
    daemon.idle(&daemon.agent).await;
    no_send_reviewed(&classifier);
    drop(connection);
    daemon.stop().await;
}

/// Two kept members connected, then the daemon restarts before either
/// messages the other: the edge was kept (the engine restores the mob's
/// wiring) and the message passes without review. Then one end is
/// retired: it is gone from the other's `peers`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_outlives_a_restart_and_goes_with_a_retired_end() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let user = common::user_texts(messages).concat();
        let all = format!("{messages:?}");
        if user.contains("TASK-OF-X") {
            if user.contains("PEERS-AGAIN") {
                return if all.contains("\"px2\"") {
                    Step::Text("X-LISTED")
                } else {
                    tool("px2", "peers", json!({}))
                };
            }
            if user.contains("TELL-Y") {
                if after_peers(messages) {
                    return send("mx", peer_id(messages, "main/y"), "FROM-X-AFTER-RESTART");
                }
                if answered(messages, "mx") {
                    return Step::Text("X-SENT");
                }
                return tool("px", "peers", json!({}));
            }
            return Step::Text("X-IDLE");
        }
        if user.contains("TASK-OF-Y") {
            if all.contains("FROM-X-AFTER-RESTART") {
                return Step::Text("Y-HEARD-X");
            }
            return Step::Text("Y-IDLE");
        }
        // The agent: its prompts say what to do.
        let last = common::user_texts(messages)
            .last()
            .cloned()
            .unwrap_or_default();
        if last.contains("SPAWN") {
            if all.contains("\"c1\"") {
                return Step::Text("connected");
            }
            if all.contains("\"s2\"") {
                return connect("c1", &["x", "y"]);
            }
            return Step::Tools(vec![
                (
                    "s1",
                    "spawn_member",
                    json!({"profile": "main", "member_id": "x", "initial_message": "TASK-OF-X", "auto_wire_parent": true}),
                ),
                (
                    "s2",
                    "spawn_member",
                    json!({"profile": "main", "member_id": "y", "initial_message": "TASK-OF-Y", "auto_wire_parent": true}),
                ),
            ]);
        }
        Step::Text("ok")
    }));
    let llm = ScriptedLlmClient::new(vec![step.clone(); 30]);
    let (daemon, _classifier) = strict(&ws, &llm).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "SPAWN").await;
    common::until(|| {
        heard(&llm, "SPAWN", "Your helper `x` went idle")
            && heard(&llm, "SPAWN", "Your helper `y` went idle")
    })
    .await;
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;

    let llm = ScriptedLlmClient::new(vec![step; 30]);
    let (daemon, classifier) = strict(&ws, &llm).await;
    let connection = daemon.connect_bound().await;
    // The member `x` is told (by its owner's client) to message `y`.
    connection
        .call(
            "mob/member_send",
            json!({"mob_id": "tui", "agent_identity": "x", "content": "TELL-Y"}),
        )
        .await
        .unwrap();
    common::until(|| heard(&llm, "TASK-OF-Y", "FROM-X-AFTER-RESTART")).await;
    // Not `no_send_reviewed`: a restored helper's calls are judged before
    // its tool catalog is read (at its first run's end), as foreign tools,
    // so this first send went to the classifier, which allowed it.
    drop(classifier);
    // `y` retires: `x` no longer lists it.
    daemon
        .state()
        .mob_retire(
            &meerkat_mob::MobId::from("tui"),
            meerkat_mob::AgentIdentity::from("y"),
        )
        .await
        .unwrap();
    common::until_async(async || {
        let members = connection
            .call("mob/members", json!({"mob_id": "tui"}))
            .await
            .unwrap();
        !members.to_string().contains("\"y\"")
    })
    .await;
    connection
        .call(
            "mob/member_send",
            json!({"mob_id": "tui", "agent_identity": "x", "content": "PEERS-AGAIN"}),
        )
        .await
        .unwrap();
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| r.user().contains("TASK-OF-X") && result_of(&r.messages, "px2").is_some())
    })
    .await;
    let listing = llm
        .recorded()
        .into_iter()
        .filter(|r| r.user().contains("TASK-OF-X"))
        .find_map(|r| {
            let (text, _) = result_of(&r.messages, "px2")?;
            Some(text)
        })
        .unwrap();
    assert!(!listing.contains("main/y"), "{listing}");
    assert!(listing.contains("main-"), "its parent stays: {listing}");
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}
