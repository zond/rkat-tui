//! `assign`: a single-use helper beside its caller (the owner's decision of
//! 2026-11-16). The call returns at once; the helper and its parent are
//! wired both ways; the parent is told when the helper finishes, with its
//! answer, and the helper is then retired; a message from the parent before
//! that makes it go on.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::Running;
use serde_json::json;

/// The text of the last message of `messages`, whatever its kind.
fn last(messages: &[Message]) -> String {
    messages
        .last()
        .map(|m| format!("{m:?}"))
        .unwrap_or_default()
}

/// The peer id of the peer whose name contains `needle`, from the last
/// `peers` result in `messages`.
pub fn peer_id(messages: &[Message], needle: &str) -> Option<String> {
    messages.iter().rev().find_map(|m| match m {
        Message::ToolResults { results, .. } => results.iter().find_map(|r| {
            let value: serde_json::Value = serde_json::from_str(&r.text_content()).ok()?;
            value["peers"].as_array()?.iter().find_map(|peer| {
                peer["name"]
                    .as_str()
                    .is_some_and(|name| name.contains(needle))
                    .then(|| peer["peer_id"].as_str().map(str::to_string))?
            })
        }),
        _ => None,
    })
}

/// Whether the last message is a tool result holding a `peers` listing.
fn after_peers(messages: &[Message]) -> bool {
    matches!(messages.last(), Some(Message::ToolResults { results, .. })
        if results.iter().any(|r| r.text_content().contains("\"peers\"")))
}

fn assign(name: &str, task: &str) -> Step {
    Step::Tool {
        id: "a1",
        name: "assign",
        args: json!({"name": name, "task": task}),
    }
}

fn send(peer: Option<String>, body: &str) -> Step {
    Step::Tool {
        id: "m1",
        name: "send_message",
        args: json!({"peer_id": peer.unwrap_or_default(), "body": body, "handling_mode": "queue"}),
    }
}

fn peers() -> Step {
    Step::Tool {
        id: "p1",
        name: "peers",
        args: json!({}),
    }
}

/// Whether one recorded request of `side` (the helper's: its task names
/// `HELPER-TASK`) carries `text`.
fn heard(llm: &ScriptedLlmClient, helper: bool, text: &str) -> bool {
    llm.recorded()
        .iter()
        .filter(|r| r.user().contains("HELPER-TASK") == helper)
        .any(|r| format!("{:?}", r.messages).contains(text))
}

async fn members(connection: &rkat_tui::client::transport::Connection) -> Vec<String> {
    let members = connection
        .call("mob/members", json!({"mob_id": "tui"}))
        .await
        .unwrap();
    members["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| member["agent_identity"].as_str().unwrap().to_string())
        .collect()
}

/// The whole life of an `assign` helper, each claim shown positively:
///
/// - the call returns at once: the parent's next model call is made while
///   the helper's first one is still held;
/// - duplex: the helper's message is read by the parent's model, the
///   parent's by the helper's;
/// - a message from the parent before the helper finished makes it go on:
///   its first run's end (`FIRST-PART`) is not its finish, and it answers
///   the message in a run after it;
/// - told: the parent's next request carries the note with the answer it
///   gave last (`HELPER-ANSWER`), not the first part;
/// - retired: the mob's roster no longer lists it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_assigned_helper_runs_beside_its_parent_talks_both_ways_and_retires_when_done() {
    let ws = Workspace::new();
    let first = Arc::new(tokio::sync::Notify::new());
    let second = Arc::new(tokio::sync::Notify::new());
    let step = {
        let (first, second) = (Arc::clone(&first), Arc::clone(&second));
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let user = common::user_texts(messages).concat();
            let tail = last(messages);
            if user.contains("HELPER-TASK") {
                if after_peers(messages) {
                    return send(peer_id(messages, "main-"), "HELLO-PARENT");
                }
                if matches!(messages.last(), Some(Message::ToolResults { .. })) {
                    return Step::Gated(Arc::clone(&second), Box::new(Step::Text("FIRST-PART")));
                }
                if tail.contains("HELLO-HELPER") {
                    return Step::Text("HELPER-ANSWER");
                }
                return Step::Gated(Arc::clone(&first), Box::new(peers()));
            }
            if after_peers(messages) {
                return send(peer_id(messages, "scout"), "HELLO-HELPER");
            }
            if matches!(messages.last(), Some(Message::ToolResults { .. })) {
                return Step::Text("PARENT-DONE");
            }
            if tail.contains("finished its task") {
                return Step::Text("NOTED");
            }
            if tail.contains("HELLO-PARENT") {
                return peers();
            }
            assign("scout", "HELPER-TASK: report back")
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 30]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    // Returned at once: the parent's call after the result, made while the
    // helper's first call is still held.
    common::until(|| heard(&llm, false, "is running. You will be told")).await;
    let result = llm
        .recorded()
        .iter()
        .find(|r| {
            !r.user().contains("HELPER-TASK")
                && matches!(r.messages.last(), Some(Message::ToolResults { .. }))
        })
        .map(|r| last(&r.messages))
        .unwrap();
    assert!(
        result.contains("Helper `scout` is running")
            && result.contains("with `send_message` (it is `tui/main/scout` in `peers`)"),
        "{result}"
    );
    // The helper's first call cannot answer before `first` is notified,
    // which only happens now: the parent's call above came while the
    // helper had not got past it.
    common::until(|| heard(&llm, true, "HELPER-TASK")).await;
    assert!(
        !heard(&llm, true, "\"peers\""),
        "the helper's first call is still held"
    );
    first.notify_one();
    // Duplex: the parent's model reads the helper's message, and answers it.
    common::until(|| heard(&llm, false, "HELLO-PARENT")).await;
    common::until(|| {
        llm.recorded().iter().any(|r| {
            !r.user().contains("HELPER-TASK")
                && matches!(r.messages.last(), Some(Message::ToolResults { .. }))
                && format!("{:?}", r.messages).contains("HELLO-HELPER")
        })
    })
    .await;
    // The parent's message was taken before the helper's first run ends.
    second.notify_one();
    common::until(|| heard(&llm, true, "HELLO-HELPER")).await;
    // It read the message after its first run ended (`FIRST-PART` is in
    // that request): the run's end with a message unread was no finish.
    let went_on = llm
        .recorded()
        .iter()
        .filter(|r| r.user().contains("HELPER-TASK"))
        .map(|r| format!("{:?}", r.messages))
        .find(|m| m.contains("HELLO-HELPER"))
        .unwrap();
    assert!(went_on.contains("FIRST-PART"), "{went_on}");
    // Told with its last answer, and retired.
    common::until(|| heard(&llm, false, "Your helper `scout` finished its task")).await;
    let note = llm
        .recorded()
        .iter()
        .map(|r| format!("{:?}", r.messages))
        .find(|m| m.contains("finished its task"))
        .unwrap();
    assert!(note.contains("HELPER-ANSWER"), "{note}");
    common::until_async(async || !members(&connection).await.contains(&"scout".to_string())).await;
    drop(connection);
    daemon.stop().await;
}

/// A message to an `assign` helper that finished and was retired is
/// refused saying so and what to do, not with the engine's "peer not
/// found".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_to_a_finished_helper_says_it_was_retired() {
    let ws = Workspace::new();
    let answer = Arc::new(tokio::sync::Notify::new());
    let step = {
        let answer = Arc::clone(&answer);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let user = common::user_texts(messages).concat();
            if user.contains("HELPER-TASK") {
                return Step::Gated(Arc::clone(&answer), Box::new(Step::Text("HELPER-ANSWER")));
            }
            let all = format!("{messages:?}");
            if all.contains("\"m1\"") {
                return Step::Text("PARENT-DONE");
            }
            if all.contains("finished its task") {
                return send(peer_id(messages, "scout"), "MORE-WORK");
            }
            if all.contains("\"p1\"") {
                return Step::Text("WAITING");
            }
            if all.contains("\"a1\"") {
                return peers();
            }
            assign("scout", "HELPER-TASK: answer")
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    // The parent knows the helper's peer id before it finishes.
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| !r.user().contains("HELPER-TASK") && peer_id(&r.messages, "scout").is_some())
    })
    .await;
    answer.notify_one();
    common::until(|| heard(&llm, false, "MORE-WORK")).await;
    common::until(|| {
        llm.recorded().iter().any(|r| {
            matches!(r.messages.last(), Some(Message::ToolResults { results, .. })
                if results.iter().any(|res| res.tool_use_id == "m1"))
        })
    })
    .await;
    let result = llm
        .recorded()
        .iter()
        .find_map(|r| match r.messages.last() {
            Some(Message::ToolResults { results, .. }) => results
                .iter()
                .find(|res| res.tool_use_id == "m1")
                .map(meerkat_core::types::ToolResult::text_content),
            _ => None,
        })
        .unwrap();
    assert!(
        result.contains(
            "helper `scout` finished its task and was retired; start another helper for more work"
        ),
        "{result}"
    );
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}

/// The ids of the calls of each tool not offered, the agent's and its
/// helper's, in the order of `NOT_OFFERED`.
const AGENT_IDS: [&str; 6] = ["n0", "n1", "n2", "n3", "n4", "n5"];
const HELPER_IDS: [&str; 6] = ["h0", "h1", "h2", "h3", "h4", "h5"];

/// A call of every tool rkat-tui does not offer, with the ids `ids`.
fn not_offered(ids: [&'static str; 6]) -> Step {
    let args =
        json!({"member_id": "old", "task": "x", "profile": "main", "peer_id": "p", "body": "b"});
    Step::Tools(
        rkat_tui::approval::policy::NOT_OFFERED
            .into_iter()
            .zip(ids)
            .map(|(name, id)| (id, name, args.clone()))
            .collect(),
    )
}

/// The engine tools rkat-tui does not offer (`delegate`, `fork_off`,
/// `spawn_member`, `spawn_many_members`, `send_request`, `send_response`)
/// are in no request of the agent nor of its helper, and a call of each
/// that either makes all the same is refused by the engine before any
/// dispatch, the gate's included: the agent's policy denies it ("not
/// allowed by policy"), the helper at the default depth limit has none of
/// the mob ones ("Tool not found") and its policy denies the others.
/// Nothing reached the classifier, nothing was seated for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tools_not_offered_are_refused_before_the_gate() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if common::is_helper(messages, "HELPER-TASK") {
            return if common::after(messages, "h0") {
                Step::Text("HELPER-REFUSED")
            } else {
                not_offered(HELPER_IDS)
            };
        }
        if common::told_finished(messages) {
            return Step::Text("NOTED");
        }
        if common::after(messages, "a1") {
            return Step::Text("waiting");
        }
        if common::after(messages, "n0") {
            return assign("scout", "HELPER-TASK: try them");
        }
        not_offered(AGENT_IDS)
    }));
    let llm = ScriptedLlmClient::new(vec![step; 12]);
    let classifier = rig::ScriptedClassifier::allowing(Vec::new());
    let setup = rig::Setup::default().classifier(Arc::clone(&classifier) as _);
    let daemon =
        Running::start_with(&setup.command_line(&ws.root), setup.options(&ws.root, &llm)).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    common::until(|| common::parent_told(&llm, "HELPER-TASK", "scout")).await;
    daemon.idle(&daemon.agent).await;
    let recorded = llm.recorded();
    let helper = recorded
        .iter()
        .find(|r| common::is_helper(&r.messages, "HELPER-TASK"))
        .unwrap();
    // The agent has `assign`; its helper, at the default depth limit,
    // starts no helper at all.
    assert!(recorded[0].tools.iter().any(|t| t == "assign"));
    assert!(!helper.tools.iter().any(|t| t == "assign"));
    for request in [&recorded[0], helper] {
        for name in rkat_tui::approval::policy::NOT_OFFERED {
            assert!(
                !request.tools.iter().any(|t| t == name),
                "{name}: {:?}",
                request.tools
            );
        }
    }
    let refused = |id: &str| {
        recorded
            .iter()
            .find(|r| common::after(&r.messages, id))
            .map(|r| common::tool_text(&r.messages, id))
            .unwrap()
    };
    for (name, ids) in rkat_tui::approval::policy::NOT_OFFERED
        .into_iter()
        .zip(AGENT_IDS.into_iter().zip(HELPER_IDS))
    {
        for id in [ids.0, ids.1] {
            let text = refused(id);
            assert!(
                text.contains(&format!("Tool '{name}' is not allowed by policy"))
                    || text.contains(&format!("Tool not found: {name}")),
                "{id} {name}: {text}"
            );
        }
    }
    assert!(
        !classifier
            .tools()
            .iter()
            .any(|t| rkat_tui::approval::policy::NOT_OFFERED.contains(&t.as_str())),
        "{:?}",
        classifier.tools()
    );
    let seated = members(&connection).await;
    assert!(!seated.iter().any(|m| m == "old"), "{seated:?}");
    drop(connection);
    daemon.stop().await;
}

fn spawn_member(member: &str, kickoff: &str) -> Step {
    common::keep("s1", member, kickoff)
}

/// A kept fresh helper (`assign` with `keep: true`) tells its parent when
/// it goes idle, with its last message, and stays seated (the roster still
/// lists it); no note comes for its start. A message to it runs it again,
/// and it is told again with that run's answer; `retire_helper` retires it
/// (the engine's `retire_member` refuses: it lets a member retire only the
/// members a `fork_off` of its own seated).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kept_helper_s_parent_is_told_each_time_it_goes_idle_and_it_stays_until_retired() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if common::user_texts(messages)
            .concat()
            .contains("HELPER-TASK")
        {
            if format!("{messages:?}").contains("MORE-WORK") {
                return Step::Text("SECOND-ANSWER");
            }
            return Step::Text("IDLE-ANSWER");
        }
        if common::after(messages, "r1") {
            return Step::Text("retired");
        }
        if last(messages).contains("SECOND-ANSWER") {
            return Step::Tool {
                id: "r1",
                name: "retire_helper",
                args: json!({"name": "w1"}),
            };
        }
        if common::after(messages, "m1") {
            return Step::Text("sent");
        }
        if after_peers(messages) {
            return send(peer_id(messages, "main/w1"), "MORE-WORK");
        }
        if last(messages).contains("went idle") {
            return peers();
        }
        if matches!(messages.last(), Some(Message::ToolResults { .. })) {
            return Step::Text("spawned");
        }
        spawn_member("w1", "HELPER-TASK: say something")
    }));
    let llm = ScriptedLlmClient::new(vec![step; 24]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    common::until(|| heard(&llm, false, "Your helper `w1` went idle")).await;
    let started = llm
        .recorded()
        .iter()
        .find(|r| common::after(&r.messages, "s1"))
        .map(|r| common::tool_text(&r.messages, "s1"))
        .unwrap();
    assert!(
        started.contains("It is kept") && started.contains("retire_helper"),
        "{started}"
    );
    let notes: Vec<String> = llm
        .recorded()
        .iter()
        .filter(|r| !r.user().contains("HELPER-TASK"))
        .map(|r| format!("{:?}", r.messages))
        .filter(|m| m.contains("Your helper `w1`"))
        .collect();
    assert!(notes[0].contains("IDLE-ANSWER"), "{notes:?}");
    // The parent's message runs it again (so it stayed seated after its
    // first run); that run's end is told too.
    common::until(|| heard(&llm, false, "SECOND-ANSWER")).await;
    let registry = daemon.registry();
    assert_eq!(registry.endings().told(), 2, "one idle note for each run");
    // `retire_helper` ends it: the roster lists it no more.
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| common::after(&r.messages, "r1"))
    })
    .await;
    let retired = llm
        .recorded()
        .iter()
        .find(|r| common::after(&r.messages, "r1"))
        .map(|r| common::tool_text(&r.messages, "r1"))
        .unwrap();
    let roster = connection
        .call("mob/members", json!({"mob_id": "tui"}))
        .await
        .unwrap();
    assert!(
        retired.contains("Helper `w1` was retired"),
        "{retired}; {roster}"
    );
    common::until_async(async || !members(&connection).await.contains(&"w1".to_string())).await;
    drop(connection);
    daemon.stop().await;
}

/// A member whose run's last call was a message to its parent goes idle
/// untold: the parent has its last word already (it read `REPORT`), and
/// the run's end was decided (counted) without a note.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_that_just_messaged_its_parent_goes_idle_untold() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if common::user_texts(messages)
            .concat()
            .contains("HELPER-TASK")
        {
            if after_peers(messages) {
                return send(peer_id(messages, "main-"), "REPORT");
            }
            if matches!(messages.last(), Some(Message::ToolResults { .. })) {
                return Step::Text("bye");
            }
            return peers();
        }
        if last(messages).contains("REPORT") {
            return Step::Text("got the report");
        }
        if matches!(messages.last(), Some(Message::ToolResults { .. })) {
            return Step::Text("spawned");
        }
        spawn_member("w1", "HELPER-TASK: report")
    }));
    let llm = ScriptedLlmClient::new(vec![step; 12]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    common::until(|| heard(&llm, false, "REPORT")).await;
    let registry = daemon.registry();
    common::until(|| registry.endings().idle_decided() >= 1).await;
    assert_eq!(registry.endings().told(), 0);
    assert!(!heard(&llm, false, "went idle"));
    drop(connection);
    daemon.stop().await;
}

/// The name rules: a malformed name, an agent's name and a name in use are
/// each refused with what to do; nothing is seated for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn assign_refuses_a_bad_or_taken_name_saying_what_is_allowed() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if common::user_texts(messages)
            .concat()
            .contains("HELPER-TASK")
        {
            return Step::Hang;
        }
        let results = messages
            .iter()
            .filter(|m| matches!(m, Message::ToolResults { .. }))
            .count();
        match results {
            0 => Step::Tools(vec![
                ("a1", "assign", json!({"name": "Bad Name", "task": "x"})),
                ("a2", "assign", json!({"name": "main-ab12", "task": "x"})),
                (
                    "a3",
                    "assign",
                    json!({"name": "scout", "task": "HELPER-TASK: wait"}),
                ),
                (
                    "a4",
                    "assign",
                    json!({"name": "kid", "task": "x", "context": "fork", "model": "other"}),
                ),
            ]),
            1 => assign("scout", "HELPER-TASK: again"),
            _ => Step::Text("done"),
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 12]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let all = format!("{:?}", llm.recorded().last().unwrap().messages);
    for refusal in [
        "`Bad Name` is not a helper name: a name is 1 to 32 lowercase letters, digits and '-'",
        "`main-ab12` is reserved for the workspace's agents",
        "the name `scout` is in use by a member seated now",
        "call `assign` again",
        "a fork runs on your own model",
    ] {
        assert!(all.contains(refusal), "{refusal}: {all}");
    }
    assert!(all.contains("call `assign` again without `model`"), "{all}");
    let seated = members(&connection).await;
    assert!(
        !seated
            .iter()
            .any(|m| m == "main-ab12" || m == "Bad Name" || m == "kid"),
        "{seated:?}"
    );
    drop(connection);
    daemon.stop().await;
}
