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

/// Whether the last message is a tool result holding a `contacts` listing.
fn after_peers(messages: &[Message]) -> bool {
    matches!(messages.last(), Some(Message::ToolResults { results, .. })
        if results.iter().any(|r| r.text_content().contains("\"peer_id\"")))
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
    Step::Gated(Arc::clone(go), Box::new(tool(ids.0, "contacts", json!({}))))
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
    assert!(
        answer.contains("`contacts` shows who is connected"),
        "{answer}"
    );
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
        ("TASK-OF-A", "`b`: you can message it"),
        ("TASK-OF-B", "`a`: you can message it"),
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

/// The helpers the workspace's mob seats (its agents left out).
async fn seated_helpers(connection: &rkat_tui::client::transport::Connection) -> Vec<String> {
    let members = connection
        .call("mob/members", json!({"mob_id": "tui"}))
        .await
        .unwrap();
    members["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| member["agent_identity"].as_str().unwrap().to_string())
        .filter(|name| !rkat_tui::daemon::agents::is_agent(name))
        .collect()
}

/// A single-use helper connected to a sibling, messaged by it during its
/// last step and after it finished: the message during its last step is
/// unfinished input, so it reads it in one more run and then finishes; it
/// is retired once and its parent told once; the sibling's `contacts` no
/// longer lists it, and the sibling's later send is refused saying it was
/// retired; nothing is left seated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_use_helper_messaged_by_its_sibling_at_its_end_is_retired_once() {
    let ws = Workspace::new();
    let gates: Vec<Arc<tokio::sync::Notify>> = (0..3)
        .map(|_| Arc::new(tokio::sync::Notify::new()))
        .collect();
    let step = {
        let gates = gates.clone();
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let user = common::user_texts(messages).concat();
            if user.contains("TASK-OF-A") {
                if format!("{messages:?}").contains("FROM-B-1") {
                    return Step::Text("A-HEARD-B");
                }
                // Its last step, held open while b sends to it.
                return Step::Gated(Arc::clone(&gates[0]), Box::new(Step::Text("A-DONE")));
            }
            if user.contains("TASK-OF-B") {
                if answered(messages, "sb2") {
                    return Step::Text("B-DONE");
                }
                if answered(messages, "pb2") {
                    return send("sb2", peer_id(messages, "main/a"), "FROM-B-2");
                }
                if answered(messages, "sb1") {
                    return Step::Gated(
                        Arc::clone(&gates[2]),
                        Box::new(tool("pb2", "contacts", json!({}))),
                    );
                }
                if answered(messages, "pb1") {
                    return send("sb1", peer_id(messages, "main/a"), "FROM-B-1");
                }
                return Step::Gated(
                    Arc::clone(&gates[1]),
                    Box::new(tool("pb1", "contacts", json!({}))),
                );
            }
            let all = format!("{messages:?}");
            if all.contains("\"c1\"") {
                return Step::Text("NOTED");
            }
            if answered(messages, "a1") || answered(messages, "a2") {
                return connect("c1", &["a", "b"]);
            }
            Step::Tools(vec![
                ("a1", "assign", json!({"name": "a", "task": "TASK-OF-A"})),
                ("a2", "assign", json!({"name": "b", "task": "TASK-OF-B"})),
            ])
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 60]);
    let (daemon, _classifier) = strict(&ws, &llm).await;
    let connection = daemon.connect_bound().await;
    let watcher = daemon.connect().await;
    daemon.prompt(&connection, "go").await;
    let b_answered = |id: &str| {
        llm.recorded()
            .iter()
            .filter(|r| r.user().contains("TASK-OF-B"))
            .any(|r| answered(&r.messages, id))
    };
    // b lists a, then messages it while a's last step is open.
    gates[1].notify_one();
    common::until(|| b_answered("sb1")).await;
    let listed = llm
        .recorded()
        .iter()
        .find_map(|r| result_of(&r.messages, "pb1"))
        .unwrap()
        .0;
    assert!(listed.contains("\"name\":\"a\""), "{listed}");
    // a's run ends; the message is unfinished input: one more run reads
    // it, then a is done and retired.
    gates[0].notify_one();
    common::until(|| heard(&llm, "TASK-OF-A", "FROM-B-1")).await;
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    while seated_helpers(&watcher).await.contains(&"a".to_string()) {
        if tokio::time::Instant::now() >= deadline {
            let registry = daemon.registry();
            let key = rkat_tui::mobs::HelperKey::new(
                meerkat_mob::MobId::from("tui"),
                meerkat_mob::AgentIdentity::from("a"),
            );
            panic!(
                "a stays seated: tracked {}, settled {}, told {}, retires {}, activity {:?}, a's requests {}\n{}",
                registry.endings().is_tracked(&key),
                registry.endings().settled(&key),
                registry.endings().told(),
                registry.endings().retires(),
                registry.activity(&key),
                llm.recorded()
                    .iter()
                    .filter(|r| r.user().contains("TASK-OF-A"))
                    .count(),
                common::log_tail(400)
            );
        }
        tokio::time::sleep(common::POLL).await;
    }
    common::until(|| {
        llm.recorded()
            .iter()
            .filter(|r| !r.user().contains("TASK-OF-"))
            .any(|r| format!("{:?}", r.messages).contains("Your helper `a` finished"))
    })
    .await;
    // b lists its contacts again (a is gone) and messages a: refused.
    gates[2].notify_one();
    common::until(|| b_answered("sb2")).await;
    let again = llm
        .recorded()
        .iter()
        .find_map(|r| result_of(&r.messages, "pb2"))
        .unwrap()
        .0;
    assert!(!again.contains("\"name\":\"a\""), "{again}");
    let (refused, error) = llm
        .recorded()
        .iter()
        .find_map(|r| result_of(&r.messages, "sb2"))
        .unwrap();
    assert!(error, "{refused}");
    assert!(refused.contains("retired"), "{refused}");
    common::until(|| {
        llm.recorded()
            .iter()
            .filter(|r| !r.user().contains("TASK-OF-"))
            .any(|r| format!("{:?}", r.messages).contains("Your helper `b` finished"))
    })
    .await;
    common::until_async(async || seated_helpers(&watcher).await.is_empty()).await;
    daemon.idle(&daemon.agent).await;
    // Told once each, retired once each.
    let last = format!(
        "{:?}",
        llm.recorded()
            .iter()
            .rfind(|r| !r.user().contains("TASK-OF-"))
            .unwrap()
            .messages
    );
    assert_eq!(
        last.matches("Your helper `a` finished").count(),
        1,
        "{last}"
    );
    let registry = daemon.registry();
    assert_eq!(registry.endings().told(), 2);
    assert_eq!(registry.endings().retires(), 2);
    drop(connection);
    drop(watcher);
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
        common::keep("a1", "a", "TASK-OF-A")
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

/// Mixed kinds: a single-use fork and a kept fresh helper, both started
/// with `assign`, connected talk both ways, without review.
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
                    "assign",
                    json!({"name": "w", "task": "TASK-OF-W", "keep": true}),
                ),
                (
                    "f1",
                    "assign",
                    json!({"name": "kid", "task": "TASK-OF-KID", "context": "fork"}),
                ),
            ])
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 40]);
    let (daemon, classifier) = strict(&ws, &llm).await;
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

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

/// A helper driven by commands its owner's client sends it: `LIST<n>`
/// lists its peers (call `l<n>`); `SEND<n>` lists them (`q<n>`) and sends
/// `FROM-<me>-<n>` to the peer whose name holds `other` (`s<n>`).
fn commanded(messages: &[Message], me: &str, other: &str) -> Step {
    // The last command, as a user text or an external event's body.
    let all = format!("{messages:?}");
    let at = all.rfind("\"SEND").max(all.rfind("\"LIST"));
    let last = at
        .and_then(|at| {
            let rest = &all[at + 1..];
            rest.find('"').map(|end| rest[..end].to_string())
        })
        .unwrap_or_default();
    let Some(n) = last
        .strip_prefix("LIST")
        .or_else(|| last.strip_prefix("SEND"))
    else {
        return Step::Text("idle");
    };
    let list = last.starts_with("LIST");
    let (q, s) = if list {
        (leak(format!("l{n}")), leak(format!("x{n}")))
    } else {
        (leak(format!("q{n}")), leak(format!("s{n}")))
    };
    if answered(messages, s) || (list && answered(messages, q)) {
        return Step::Text("done");
    }
    if answered(messages, q) {
        let peer = peer_id(messages, other);
        return send(s, peer, &format!("FROM-{me}-{n}"));
    }
    tool(q, "contacts", json!({}))
}

/// The mob tools the agent's script calls.
const MOB_TOOLS: [&str; 3] = ["mob_create", "mob_spawn_member", "mob_destroy"];

/// `mob_spawn_member` of `member` in `team` (its task names it).
fn in_team(member: &str, task: &str) -> serde_json::Value {
    json!({"mob_id": "team", "profile": "worker", "member_id": member, "initial_message": task})
}

/// A kept fresh helper `member` in the workspace's mob (`assign`).
fn in_tui(member: &str, task: &str) -> serde_json::Value {
    json!({"name": member, "task": task, "keep": true})
}

/// A script in which the agent, prompted `go`, creates the mob `team`
/// (its members addressable by their owner's client, so the test can
/// drive them), seats `seats` in order, connects `pair` and is done;
/// prompted `DESTROY`, it destroys `team`. Each helper of `helpers`
/// (its task's marker, how it signs, the peer name it messages) does what
/// the test tells it ([`commanded`]).
fn team_script(
    seats: Vec<(&'static str, &'static str, serde_json::Value)>,
    helpers: Vec<(&'static str, &'static str, &'static str)>,
    pair: &'static [&'static str],
) -> Step {
    Step::Dynamic(Arc::new(move |messages: &[Message]| {
        let user = common::user_texts(messages).concat();
        for (marker, me, other) in &helpers {
            if user.contains(marker) {
                return commanded(messages, me, other);
            }
        }
        let all = format!("{messages:?}");
        let last = common::user_texts(messages)
            .last()
            .cloned()
            .unwrap_or_default();
        if last == "DESTROY" {
            return if all.contains("\"d1\"") {
                Step::Text("destroyed")
            } else {
                tool("d1", "mob_destroy", json!({"mob_id": "team"}))
            };
        }
        if !user.contains("go") || all.contains("\"c1\"") {
            return Step::Text("ok");
        }
        if !all.contains("\"m1\"") {
            return tool(
                "m1",
                "mob_create",
                json!({"definition": {"id": "team", "profiles": {"worker": {"model": common::MODEL, "external_addressable": true, "tools": {"comms": true}}}}}),
            );
        }
        for (id, name, args) in &seats {
            if !all.contains(&format!("\"{id}\"")) {
                return Step::Tool {
                    id,
                    name,
                    args: args.clone(),
                };
            }
        }
        connect("c1", pair)
    }))
}

/// The result of the call `id` in a request whose user text holds
/// `marker`, once there is one.
async fn result_in(llm: &ScriptedLlmClient, marker: &str, id: &str) -> (String, bool) {
    common::poll(async || {
        llm.recorded()
            .iter()
            .filter(|r| r.user().contains(marker))
            .find_map(|r| result_of(&r.messages, id))
    })
    .await
}

/// Tell `who` of `mob` (its task `marker`, signing `me`) to send message
/// `n` to its peer; the send succeeded and the request of the peer (its
/// task `to`) after it holds the message.
async fn relay(
    connection: &rkat_tui::client::transport::Connection,
    llm: &ScriptedLlmClient,
    (mob, who, marker, me): (&str, &str, &str, &str),
    n: u32,
    to: &str,
) {
    connection
        .call(
            "mob/member_send",
            json!({"mob_id": mob, "agent_identity": who, "content": format!("SEND{n}")}),
        )
        .await
        .unwrap();
    let (sent, error) = result_in(llm, marker, &format!("s{n}")).await;
    assert!(!error && sent.contains("\"sent\""), "{who}: {sent}");
    let text = format!("FROM-{me}-{n}");
    common::until(|| heard(llm, to, &text)).await;
}

/// `who` of `mob` (its task `marker`) lists its peers (`LIST<n>`): the
/// listing.
async fn listing(
    connection: &rkat_tui::client::transport::Connection,
    llm: &ScriptedLlmClient,
    (mob, who, marker): (&str, &str, &str),
    n: u32,
) -> String {
    connection
        .call(
            "mob/member_send",
            json!({"mob_id": mob, "agent_identity": who, "content": format!("LIST{n}")}),
        )
        .await
        .unwrap();
    result_in(llm, marker, &format!("l{n}")).await.0
}

/// The record of the connections across mobs.
fn record(ws: &Workspace) -> String {
    std::fs::read_to_string(common::realm_of(&ws.root).join("helper-links.json"))
        .unwrap_or_default()
}

const A: (&str, &str, &str, &str) = ("tui", "a", "TASK-OF-A", "A");
const W1: (&str, &str, &str, &str) = ("team", "w1", "TASK-OF-W1", "W1");
const W2: (&str, &str, &str, &str) = ("team", "w2", "TASK-OF-W2", "W2");

/// A helper of the workspace's mob and a member of a mob the agent
/// created, connected: each one's message reaches the other's next model
/// request, both ways, with no review; each was told of the other; the
/// pair is recorded. After a restart they still talk (the daemon's start
/// made their sockets listen again). Then the far end retires: the
/// survivor no longer lists it, is told, and the pair leaves the record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn helpers_in_different_mobs_connected_talk_both_ways_across_a_restart() {
    let ws = Workspace::new();
    let step = team_script(
        vec![
            ("s1", "assign", in_tui("a", "TASK-OF-A")),
            ("s2", "mob_spawn_member", in_team("w1", "TASK-OF-W1")),
        ],
        vec![
            ("TASK-OF-A", "A", "team/worker/w1"),
            ("TASK-OF-W1", "W1", "tui/main/a"),
        ],
        &["a", "w1"],
    );
    let llm = ScriptedLlmClient::new(vec![step.clone(); 60]);
    let (daemon, classifier) = strict(&ws, &llm).await;
    daemon.reveal(&daemon.session, &MOB_TOOLS).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let (answer, error) = result_in(&llm, "go", "c1").await;
    assert!(!error, "{answer}");
    assert!(answer.contains("`a` and `w1` (new)"), "{answer}");
    let recorded = record(&ws);
    assert!(
        recorded.contains("\"w1\"") && recorded.contains("tui/main/a"),
        "{recorded}"
    );
    relay(&connection, &llm, A, 1, "TASK-OF-W1").await;
    relay(&connection, &llm, W1, 2, "TASK-OF-A").await;
    for (marker, other) in [
        ("TASK-OF-A", "`w1`: you can message it"),
        ("TASK-OF-W1", "`a`: you can message it"),
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
    // Both ways the same tier as within one mob: no review.
    no_send_reviewed(&classifier);
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;

    let llm = ScriptedLlmClient::new(vec![step; 60]);
    let (daemon, _classifier) = strict(&ws, &llm).await;
    let connection = daemon.connect_bound().await;
    relay(&connection, &llm, A, 3, "TASK-OF-W1").await;
    relay(&connection, &llm, W1, 4, "TASK-OF-A").await;
    daemon
        .state()
        .mob_retire(
            &meerkat_mob::MobId::from("team"),
            meerkat_mob::AgentIdentity::from("w1"),
        )
        .await
        .unwrap();
    common::until(|| !record(&ws).contains("\"w1\"")).await;
    let peers = listing(&connection, &llm, ("tui", "a", "TASK-OF-A"), 5).await;
    assert!(!peers.contains("\"name\":\"w1\""), "{peers}");
    assert!(peers.contains("main-"), "its parent stays: {peers}");
    assert!(heard(
        &llm,
        "TASK-OF-A",
        "`w1`, the helper you were connected with, has left"
    ));
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}

/// The mob of one end of a connection is destroyed: the other end no
/// longer lists it and the pair leaves the record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_destroyed_mob_s_member_leaves_its_connected_peer() {
    let ws = Workspace::new();
    let step = team_script(
        vec![
            ("s1", "assign", in_tui("a", "TASK-OF-A")),
            ("s2", "mob_spawn_member", in_team("w1", "TASK-OF-W1")),
        ],
        vec![
            ("TASK-OF-A", "A", "team/worker/w1"),
            ("TASK-OF-W1", "W1", "tui/main/a"),
        ],
        &["a", "w1"],
    );
    let llm = ScriptedLlmClient::new(vec![step; 60]);
    let (daemon, _classifier) = strict(&ws, &llm).await;
    daemon.reveal(&daemon.session, &MOB_TOOLS).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    assert!(!result_in(&llm, "go", "c1").await.1);
    relay(&connection, &llm, A, 1, "TASK-OF-W1").await;
    let before = listing(&connection, &llm, ("tui", "a", "TASK-OF-A"), 2).await;
    assert!(before.contains("\"name\":\"w1\""), "{before}");
    daemon.idle(&daemon.agent).await;
    daemon.prompt(&connection, "DESTROY").await;
    common::until(|| !record(&ws).contains("\"w1\"")).await;
    let after = listing(&connection, &llm, ("tui", "a", "TASK-OF-A"), 3).await;
    assert!(!after.contains("\"name\":\"w1\""), "{after}");
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}

/// A recorded pair whose end is no member at the daemon's start (gone
/// while no daemon could follow it; here the record names a member that
/// never was) ends at that start: the survivor's edge goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recorded_connection_whose_end_is_gone_ends_at_the_next_start() {
    let ws = Workspace::new();
    let step = team_script(
        vec![
            ("s1", "assign", in_tui("a", "TASK-OF-A")),
            ("s2", "mob_spawn_member", in_team("w1", "TASK-OF-W1")),
        ],
        vec![
            ("TASK-OF-A", "A", "team/worker/w1"),
            ("TASK-OF-W1", "W1", "tui/main/a"),
        ],
        &["a", "w1"],
    );
    let llm = ScriptedLlmClient::new(vec![step.clone(); 60]);
    let (daemon, _classifier) = strict(&ws, &llm).await;
    daemon.reveal(&daemon.session, &MOB_TOOLS).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    assert!(!result_in(&llm, "go", "c1").await.1);
    let before = listing(&connection, &llm, ("tui", "a", "TASK-OF-A"), 1).await;
    assert!(before.contains("\"name\":\"w1\""), "{before}");
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
    // The record's far end is a member that is not there.
    let file = common::realm_of(&ws.root).join("helper-links.json");
    let text = record(&ws).replace("\"member\": \"w1\"", "\"member\": \"ghost\"");
    assert!(text.contains("ghost"), "{text}");
    std::fs::write(&file, text).unwrap();

    let llm = ScriptedLlmClient::new(vec![step; 60]);
    let (daemon, _classifier) = strict(&ws, &llm).await;
    let connection = daemon.connect_bound().await;
    assert!(!record(&ws).contains("ghost"), "{}", record(&ws));
    let after = listing(&connection, &llm, ("tui", "a", "TASK-OF-A"), 2).await;
    assert!(!after.contains("\"name\":\"w1\""), "{after}");
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}

/// Two `mob_spawn_member` members of one mob the agent created,
/// connected: they talk both ways without review; in plan mode too (a
/// message between helpers of one agent is tier 1 there as well: plan
/// mode asks only before a message to an agent of another workspace).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_members_of_a_created_mob_connected_talk_both_ways_in_plan_mode_too() {
    let ws = Workspace::new();
    let step = team_script(
        vec![
            ("s1", "mob_spawn_member", in_team("w1", "TASK-OF-W1")),
            ("s2", "mob_spawn_member", in_team("w2", "TASK-OF-W2")),
        ],
        vec![
            ("TASK-OF-W1", "W1", "team/worker/w2"),
            ("TASK-OF-W2", "W2", "team/worker/w1"),
        ],
        &["w1", "w2"],
    );
    let llm = ScriptedLlmClient::new(vec![step; 60]);
    let (daemon, classifier) = strict(&ws, &llm).await;
    daemon.reveal(&daemon.session, &MOB_TOOLS).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let (answer, error) = result_in(&llm, "go", "c1").await;
    assert!(!error, "{answer}");
    assert!(answer.contains("`w1` and `w2` (new)"), "{answer}");
    relay(&connection, &llm, W1, 1, "TASK-OF-W2").await;
    relay(&connection, &llm, W2, 2, "TASK-OF-W1").await;
    // An edge of one mob: nothing for the daemon to record.
    assert!(!record(&ws).contains("\"w2\""), "{}", record(&ws));
    connection
        .call("rkat/set_mode", json!({"mode": "plan"}))
        .await
        .unwrap();
    relay(&connection, &llm, W1, 3, "TASK-OF-W2").await;
    relay(&connection, &llm, W2, 4, "TASK-OF-W1").await;
    no_send_reviewed(&classifier);
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}

/// Another agent's helper, named end to end: refused, saying whose it is
/// not, and nothing is connected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_agent_s_helper_is_refused_end_to_end() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let user = common::user_texts(messages).concat();
        if user.contains("TASK-OF-") {
            return Step::Text("helper idle");
        }
        let all = format!("{messages:?}");
        if user.contains("SECOND-AGENT") {
            return if all.contains("\"x1\"") {
                Step::Text("seated x")
            } else {
                tool("x1", "assign", in_tui("x", "TASK-OF-X"))
            };
        }
        if user.contains("FIRST-AGENT") {
            if all.contains("\"c1\"") {
                return Step::Text("done");
            }
            if all.contains("\"a1\"") {
                return connect("c1", &["a", "x"]);
            }
            return tool("a1", "assign", in_tui("a", "TASK-OF-A"));
        }
        Step::Text("ok")
    }));
    let llm = ScriptedLlmClient::new(vec![step; 40]);
    let (daemon, _classifier) = strict(&ws, &llm).await;
    let first = daemon.connect_bound().await;
    let (second, _notes) = rkat_tui::client::transport::Connection::attach_to(
        &daemon.endpoint(),
        std::time::Duration::from_secs(5),
        None,
        false,
        Some(rkat_tui::daemon::protocol::AgentChoice::New),
    )
    .await
    .unwrap();
    second.call("initialize", json!({})).await.unwrap();
    assert_ne!(first.agent(), second.agent());
    daemon.prompt(&second, "SECOND-AGENT").await;
    common::until(|| {
        daemon
            .registry()
            .bound_members()
            .iter()
            .any(|(key, _)| key.member.as_str() == "x")
    })
    .await;
    daemon.prompt(&first, "FIRST-AGENT").await;
    let (text, error) = result_in(&llm, "FIRST-AGENT", "c1").await;
    assert!(error, "{text}");
    assert!(
        text.contains(
            "`x` is not a helper you started (it is another agent's or another helper's)"
        ),
        "{text}"
    );
    assert!(text.contains("your running helpers are `a`"), "{text}");
    let members = daemon
        .state()
        .mob_list_members(&meerkat_mob::MobId::from("tui"))
        .await
        .unwrap();
    let a = members
        .iter()
        .find(|m| m.agent_identity.as_str() == "a")
        .unwrap();
    assert!(
        !a.wired_to.contains(&meerkat_mob::AgentIdentity::from("x")),
        "{:?}",
        a.wired_to
    );
    daemon.idle(first.agent()).await;
    daemon.idle(second.agent()).await;
    drop(first);
    drop(second);
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
                    tool("px2", "contacts", json!({}))
                };
            }
            if user.contains("TELL-Y") {
                if after_peers(messages) {
                    return send("mx", peer_id(messages, "main/y"), "FROM-X-AFTER-RESTART");
                }
                if answered(messages, "mx") {
                    return Step::Text("X-SENT");
                }
                return tool("px", "contacts", json!({}));
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
                ("s1", "assign", in_tui("x", "TASK-OF-X")),
                ("s2", "assign", in_tui("y", "TASK-OF-Y")),
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
    assert!(!listing.contains("\"name\":\"y\""), "{listing}");
    assert!(listing.contains("main-"), "its parent stays: {listing}");
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}
