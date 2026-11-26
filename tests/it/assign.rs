//! `assign`: a single-use helper beside its caller (the owner's
//! decision). The call returns at once; the helper and its parent are
//! wired both ways; the parent is told when the helper finishes, with its
//! answer, and the helper is then retired; a message from the parent before
//! that makes it go on.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::script::{Script, call, hold, make};
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

/// The peer id of the peer `needle` names (a member name, or a comms name
/// `<mob>/<role>/<member>` or its tail, whose member is matched), from
/// the last `contacts` result in `messages`.
pub fn peer_id(messages: &[Message], needle: &str) -> Option<String> {
    fn find(value: &serde_json::Value, matches: &dyn Fn(&str) -> bool) -> Option<String> {
        match value {
            serde_json::Value::Object(map) => {
                if map
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(matches)
                    && let Some(id) = map.get("peer_id").and_then(serde_json::Value::as_str)
                {
                    return Some(id.to_string());
                }
                map.values().find_map(|v| find(v, matches))
            }
            serde_json::Value::Array(items) => items.iter().find_map(|v| find(v, matches)),
            _ => None,
        }
    }
    let member = needle.rsplit('/').next().unwrap_or(needle);
    messages.iter().rev().find_map(|m| match m {
        Message::ToolResults { results, .. } => results.iter().find_map(|r| {
            let value: serde_json::Value = serde_json::from_str(&r.text_content()).ok()?;
            // The member named exactly, else (`main-`) the one it opens.
            find(&value, &|name| name == member)
                .or_else(|| find(&value, &|name| name.starts_with(member)))
        }),
        _ => None,
    })
}

/// The messages after the last assistant message, as one text.
fn this_round(messages: &[Message]) -> String {
    let from = messages
        .iter()
        .rposition(|m| matches!(m, Message::BlockAssistant(_)))
        .map_or(0, |at| at + 1);
    format!("{:?}", &messages[from..])
}

fn after_peers(messages: &[Message]) -> bool {
    matches!(messages.last(), Some(Message::ToolResults { results, .. })
        if results.iter().any(|r| r.text_content().contains("\"peer_id\"")))
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
        name: "contacts",
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
    common::members(connection, "tui").await
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
                // From what the request answers: the parent's message can
                // land beside a tool result, before or after it.
                if common::answers(messages, Some("m1")) {
                    return Step::Gated(Arc::clone(&second), Box::new(Step::Text("FIRST-PART")));
                }
                if common::answers(messages, Some("p1")) {
                    return send(peer_id(messages, "main-"), "HELLO-PARENT");
                }
                if tail.contains("HELLO-HELPER") || this_round(messages).contains("HELLO-HELPER") {
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
            && result.contains("with `send_message` (`contacts` gives its peer_id)"),
        "{result}"
    );
    // The helper's first call cannot answer before `first` is notified,
    // which only happens now: the parent's call above came while the
    // helper had not got past it.
    common::until(|| heard(&llm, true, "HELPER-TASK")).await;
    assert!(
        !heard(&llm, true, r#"\"you\":"#),
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
    let helper = Script::new()
        .after("h0", Step::Text("HELPER-REFUSED"))
        .otherwise(not_offered(HELPER_IDS));
    let llm = Script::new()
        .helper("HELPER-TASK", helper)
        .told_finished(Step::Text("NOTED"))
        .after("a1", Step::Text("waiting"))
        .after("n0", assign("scout", "HELPER-TASK: try them"))
        .otherwise(not_offered(AGENT_IDS))
        .client(12);
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

/// The result of the call `id` in the first request that answers it
/// (every id is one caller's); empty until one does.
fn result_of(llm: &ScriptedLlmClient, id: &str) -> String {
    llm.recorded()
        .iter()
        .find(|r| common::after(&r.messages, id))
        .map(|r| common::tool_text(&r.messages, id))
        .unwrap_or_default()
}

/// An `assign` call `id` of a kept helper `name` that answers at once and
/// stays seated, idle (it holds a slot; a helper whose model call never
/// returns would hold the daemon's stop, see `docs/testing.md`).
fn kept_call(id: &'static str, name: &str) -> (&'static str, &'static str, serde_json::Value) {
    (
        id,
        "assign",
        json!({"name": name, "task": "HELPER-KEPT: answer", "keep": true}),
    )
}

/// `helpers.max_live` (3 here: the boundary needs no more sessions) on a
/// whole run: at 2 of 3, of two `assign` calls in one step exactly one is
/// seated; `contacts` says `3 of 3 helpers`; the next call is refused with
/// the limit's text; after a `retire_helper` the next succeeds, and after
/// a single-use helper finishes, too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_helper_past_the_limit_is_refused_until_one_is_retired_or_finishes() {
    let ws = Workspace::new();
    let finish = Arc::new(tokio::sync::Notify::new());
    let llm = Script::new()
        .helper("HELPER-KEPT", Step::Text("KEPT-ANSWER"))
        .helper("HELPER-SINGLE", hold(&finish, Step::Text("SINGLE-ANSWER")))
        .after("b3", Step::Text("done"))
        .told_finished(Step::Tools(vec![kept_call("b3", "y12")]))
        .after("b2", Step::Text("waiting"))
        .after("b1", Step::Tools(vec![kept_call("b2", "y11")]))
        .after("r1", Step::Tools(vec![kept_call("b1", "y10")]))
        .after("a9", call("r1", "retire_helper", json!({"name": "k0"})))
        .after("p1", Step::Tools(vec![kept_call("a9", "y9")]))
        .after("c1", peers())
        .after(
            "a0",
            Step::Tools(vec![kept_call("c1", "x1"), kept_call("c2", "x2")]),
        )
        .input(
            "START-LIMIT",
            Step::Tools(vec![
                kept_call("a0", "k0"),
                (
                    "a1",
                    "assign",
                    json!({"name": "s0", "task": "HELPER-SINGLE: answer when told"}),
                ),
            ]),
        )
        .otherwise(Step::Text("noted"))
        .client(60);
    let setup = rig::Setup::args(&["--set", "helpers.max_live=3"]);
    let daemon =
        Running::start_with(&setup.command_line(&ws.root), setup.options(&ws.root, &llm)).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "START-LIMIT").await;
    common::until(|| !result_of(&llm, "c1").is_empty()).await;
    // Two calls of one step at 2 of 3: exactly one is seated.
    let pair = [result_of(&llm, "c1"), result_of(&llm, "c2")];
    let started = pair
        .iter()
        .filter(|text| text.contains("is running"))
        .count();
    let refused: Vec<&String> = pair
        .iter()
        .filter(|text| text.starts_with("you hold 3 helpers"))
        .collect();
    assert_eq!((started, refused.len()), (1, 1), "{pair:?}");
    common::until(|| !result_of(&llm, "p1").is_empty()).await;
    let contacts = result_of(&llm, "p1");
    assert!(
        contacts.contains(r#""helper_limit":"3 of 3 helpers""#),
        "{contacts}"
    );
    // The next: refused with the limit, naming the helpers and what to do.
    common::until(|| !result_of(&llm, "a9").is_empty()).await;
    let next = result_of(&llm, "a9");
    assert!(
        next.starts_with("you hold 3 helpers")
            && next.contains("which is this workspace's limit (helpers.max_live): ")
            && next.contains("k0")
            && next.contains("s0")
            && next.contains("kept and idle")
            && next.ends_with(
                "Retire with retire_helper the ones you no longer need, or wait for a single-use one to finish, then call assign again."
            ),
        "{next}"
    );
    common::until(|| !result_of(&llm, "r1").is_empty()).await;
    assert!(
        result_of(&llm, "r1").contains("Helper `k0` was retired"),
        "{}",
        result_of(&llm, "r1")
    );
    common::until(|| !result_of(&llm, "b1").is_empty()).await;
    assert!(
        result_of(&llm, "b1").contains("Helper `y10` is running"),
        "{}",
        result_of(&llm, "b1")
    );
    common::until(|| !result_of(&llm, "b2").is_empty()).await;
    assert!(
        result_of(&llm, "b2").starts_with("you hold 3 helpers"),
        "{}",
        result_of(&llm, "b2")
    );
    // The single-use helper finishes: its slot is free again.
    finish.notify_one();
    common::until(|| !result_of(&llm, "b3").is_empty()).await;
    assert!(
        result_of(&llm, "b3").contains("Helper `y12` is running"),
        "{}",
        result_of(&llm, "b3")
    );
    let seated = members(&connection).await;
    assert!(
        !seated
            .iter()
            .any(|m| m == "y9" || m == "y11" || m == "k0" || m == "s0"),
        "{seated:?}"
    );
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}

/// Two `assign` calls of one step for one name: one is seated, the other
/// refused with rkat-tui's own words (the name is held from before the
/// spawn), never the engine's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_calls_for_one_name_in_one_step_seat_one() {
    let ws = Workspace::new();
    let llm = Script::new()
        .helper("HELPER-KEPT", Step::Text("KEPT-ANSWER"))
        .after("n1", Step::Text("done"))
        .input(
            "START-TWINS",
            Step::Tools(vec![kept_call("n1", "twin"), kept_call("n2", "twin")]),
        )
        .otherwise(Step::Text("noted"))
        .client(12);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "START-TWINS").await;
    common::until(|| !result_of(&llm, "n2").is_empty()).await;
    let pair = [result_of(&llm, "n1"), result_of(&llm, "n2")];
    let started = pair
        .iter()
        .filter(|text| text.contains("is running"))
        .count();
    let refused = pair
        .iter()
        .filter(|text| {
            text.as_str()
                == "the name `twin` is in use by a member seated now (`contacts` lists your helpers); call `assign` again with another name"
        })
        .count();
    assert_eq!((started, refused), (1, 1), "{pair:?}");
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}

/// Each parent has a limit of its own: with `helpers.max_live = 1` the
/// agent's one helper starts one of its own, and only one; the agent's
/// second is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_s_limit_is_its_own() {
    let ws = Workspace::new();
    // The lead finishes only once the test let it: until then it holds
    // the agent's one slot.
    let release = Arc::new(tokio::sync::Notify::new());
    let lead = Script::new()
        .after("l2", hold(&release, Step::Text("LEAD-DONE")))
        .after("l1", Step::Tools(vec![kept_call("l2", "sub2")]))
        .input("LEAD-TASK", Step::Tools(vec![kept_call("l1", "sub1")]))
        .otherwise(Step::Text("lead noted"));
    let llm = Script::new()
        .helper("HELPER-KEPT", Step::Text("KEPT-ANSWER"))
        .helper("LEAD-TASK", lead)
        .told_finished(Step::Text("noted the lead"))
        .after("g2", Step::Text("done"))
        .after("g1", Step::Tools(vec![kept_call("g2", "second")]))
        .input(
            "START-LEAD",
            common::assign("g1", "lead", "LEAD-TASK: start helpers"),
        )
        .otherwise(Step::Text("noted"))
        .client(20);
    let setup = rig::Setup::args(&[
        "--set",
        "helpers.max_depth=2",
        "--set",
        "helpers.max_live=1",
    ]);
    let daemon =
        Running::start_with(&setup.command_line(&ws.root), setup.options(&ws.root, &llm)).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "START-LEAD").await;
    common::until(|| !result_of(&llm, "g2").is_empty()).await;
    assert!(
        result_of(&llm, "g2").starts_with(
            "you hold 1 helper, which is this workspace's limit (helpers.max_live): lead."
        ),
        "{}",
        result_of(&llm, "g2")
    );
    common::until(|| !result_of(&llm, "l2").is_empty()).await;
    assert!(
        result_of(&llm, "l1").contains("Helper `sub1` is running"),
        "{}",
        result_of(&llm, "l1")
    );
    // (Whether `sub1` is idle yet when it is counted depends on timing.)
    let l2 = result_of(&llm, "l2");
    assert!(
        l2.starts_with("you hold 1 helper")
            && l2.contains("which is this workspace's limit (helpers.max_live): sub1."),
        "{}",
        result_of(&llm, "l2")
    );
    // The lead finishes (its kept helper goes with it), then the stop.
    release.notify_one();
    common::until(|| common::parent_told(&llm, "LEAD-TASK", "lead")).await;
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}

/// `helpers.max_live` lowered below what the agent holds applies at once:
/// the next `assign` is refused saying it holds more than the limit, and
/// none of its helpers is retired.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lowered_limit_refuses_new_helpers_and_retires_none() {
    let ws = Workspace::new();
    let llm = Script::new()
        .helper("HELPER-KEPT", Step::Text("KEPT-ANSWER"))
        .after("z1", Step::Text("done"))
        .after("h1", Step::Text("done"))
        .input("ONE-MORE", Step::Tools(vec![kept_call("z1", "z")]))
        .input(
            "START-THREE",
            Step::Tools(vec![
                kept_call("h1", "w1"),
                kept_call("h2", "w2"),
                kept_call("h3", "w3"),
            ]),
        )
        .otherwise(Step::Text("noted"))
        .client(20);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "START-THREE").await;
    let set = connection
        .call(
            "rkat/set_setting",
            json!({"key": "helpers.max_live", "value": "2", "user": false}),
        )
        .await
        .unwrap();
    assert!(
        set["text"].as_str().unwrap().contains(
            "from now on each agent and each helper may hold 2 helpers at once from `assign`; none seated now is retired"
        ),
        "{set}"
    );
    // The kept helpers' notes start runs of their own: the prompt's run
    // may end after the one `prompt` saw end.
    daemon.prompt(&connection, "ONE-MORE").await;
    common::until(|| !result_of(&llm, "z1").is_empty()).await;
    let z1 = result_of(&llm, "z1");
    assert!(
        z1.starts_with("you hold 3 helpers")
            && z1.contains(
                "which is more than this workspace's limit of 2 (helpers.max_live): w1, w2, w3."
            ),
        "{z1}"
    );
    let seated = members(&connection).await;
    for helper in ["w1", "w2", "w3"] {
        assert!(seated.iter().any(|m| m == helper), "{helper}: {seated:?}");
    }
    assert!(!seated.iter().any(|m| m == "z"), "{seated:?}");
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}

/// The owner's report: a helper's `send_message` to its own
/// parent agent went to the classifier, although a message to an agent
/// peer is tier 1. Each kind of helper sends its parent a message with
/// the peer id `contacts` gives it: a fresh single-use one, a second one
/// under the same name once the first was retired, a kept one (twice: its
/// task, then a new task its parent sends it), and a fork. None is
/// reviewed; each is allowed by the policy as a message to the parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_s_message_to_its_parent_is_not_reviewed() {
    let ws = Workspace::new();
    let helper = Script::new()
        .after("p1", make(|m| send(peer_id(m, "main-"), "TO-PARENT")))
        .after("m1", Step::Text("HELPER-DONE"))
        .otherwise(peers());
    let more = make(|m| {
        call(
            "m9",
            "send_message",
            json!({"peer_id": peer_id(m, "keeper").unwrap_or_default(), "body": "HTASK more", "handling_mode": "queue"}),
        )
    });
    let llm = Script::new()
        .mentions("HTASK", helper)
        .after("p9", more)
        .when(|m| common::answers(m, None), Step::Text("PARENT-OK"))
        .input("go fresh", common::assign("a1", "scout", "HTASK fresh"))
        .input("go kept", common::keep("k1", "keeper", "HTASK kept"))
        .input("go more", call("p9", "contacts", json!({})))
        .input("go fork", common::fork("f1", "kid", "HTASK fork", false))
        .otherwise(Step::Text("NOTED"))
        .client(80);
    let classifier = rig::ScriptedClassifier::allowing(vec![]);
    let setup = rig::Setup::default().classifier(Arc::clone(&classifier) as _);
    let daemon =
        Running::start_with(&common::args_for(&ws.root), setup.options(&ws.root, &llm)).await;
    let connection = daemon.connect_bound().await;
    let audit = common::realm_of(&ws.root).join("audit.jsonl");
    // The helpers' sends the policy allowed, as the audit log has them.
    let allowed = || {
        std::fs::read_to_string(&audit)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|line| {
                line["tool"] == "send_message"
                    && line["helper"].is_string()
                    && line["decider"] == "policy"
                    && line["verdict"] == "allow"
                    && line["argument"].to_string().contains("TO-PARENT")
            })
            .count()
    };
    let reviewed = || {
        classifier
            .tools()
            .iter()
            .filter(|tool| *tool == "send_message")
            .count()
    };
    let gone = async |name: &str| {
        common::until_async(async || !members(&connection).await.contains(&name.to_string())).await;
    };
    for (prompt, sent, wait_gone) in [
        ("go fresh", 1, Some("scout")),
        ("go fresh again", 2, Some("scout")),
        ("go kept", 3, None),
        ("go more", 4, None),
        ("go fork", 5, Some("kid")),
    ] {
        daemon.prompt(&connection, prompt).await;
        common::until(|| allowed() >= sent || reviewed() > 0).await;
        assert_eq!(reviewed(), 0, "{prompt}: {:?}", classifier.tools());
        assert_eq!(allowed(), sent, "{prompt}");
        if let Some(name) = wait_gone {
            gone(name).await;
        }
        daemon.idle(&daemon.agent).await;
    }
    drop(connection);
    daemon.stop().await;
}

/// The sender of the newest peer message in `messages` holding `body`, as
/// the message names it (`Peer message from <sender>:`).
fn sender_of(messages: &[Message], body: &str) -> Option<String> {
    messages.iter().rev().find_map(|m| {
        let Message::SystemNotice(_) = m else {
            return None;
        };
        let text = format!("{m:?}").replace("\\n", "\n");
        let at = text.find("Peer message from ")?;
        let rest = &text[at + "Peer message from ".len()..];
        let (sender, said) = rest.split_once(":\n")?;
        said.starts_with(body).then(|| sender.to_string())
    })
}

/// The peer id the last `contacts` result in `messages` gives for the
/// peer a message named `sender` (its name, or a comms name ending in it).
fn peer_named(messages: &[Message], sender: &str) -> Option<String> {
    fn find(value: &serde_json::Value, sender: &str) -> Option<String> {
        match value {
            serde_json::Value::Object(map) => {
                if let (Some(name), Some(id)) = (
                    map.get("name").and_then(serde_json::Value::as_str),
                    map.get("peer_id").and_then(serde_json::Value::as_str),
                ) && (name == sender || sender.ends_with(&format!("/{name}")))
                {
                    return Some(id.to_string());
                }
                map.values().find_map(|v| find(v, sender))
            }
            serde_json::Value::Array(items) => items.iter().find_map(|v| find(v, sender)),
            _ => None,
        }
    }
    messages.iter().rev().find_map(|m| match m {
        Message::ToolResults { results, .. } => results.iter().find_map(|r| {
            serde_json::from_str::<serde_json::Value>(&r.text_content())
                .ok()
                .and_then(|v| find(&v, sender))
        }),
        _ => None,
    })
}

/// With `reply_to_peer` withdrawn, a message is answered with
/// `send_message`: an agent answers its helper's message, and the helper
/// the agent's answer, each to the peer id `contacts` gives for the name
/// the received message shows (no other lookup).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_is_answered_to_the_sender_it_names() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let helper = common::user_texts(messages)
            .concat()
            .contains("HELPER-TASK");
        // (what this side answers, with what, its contacts and send ids)
        let (heard, reply, (c, m)) = if helper {
            ("ANSWER", "ACK-ANSWER", ("hc", "hm"))
        } else {
            ("QUESTION", "ANSWER", ("ac", "am"))
        };
        if common::answers(messages, Some(m)) {
            return Step::Text("REPLIED");
        }
        if common::answers(messages, Some(c)) {
            let sender = sender_of(messages, heard).unwrap_or_default();
            return match peer_named(messages, &sender) {
                Some(peer) => Step::Tool {
                    id: m,
                    name: "send_message",
                    args: json!({"peer_id": peer, "body": reply, "handling_mode": "queue"}),
                },
                None => Step::Text("NO-MATCH"),
            };
        }
        if helper {
            if sender_of(messages, "ANSWER").is_some()
                && !format!("{messages:?}").contains("\"hm\"")
            {
                return Step::Tool {
                    id: c,
                    name: "contacts",
                    args: json!({}),
                };
            }
            if common::answers(messages, Some("p1")) {
                return send(peer_id(messages, "main-"), "QUESTION");
            }
            if common::answers(messages, Some("m1")) {
                return Step::Text("ASKED");
            }
            return peers();
        }
        if sender_of(messages, "QUESTION").is_some() && !format!("{messages:?}").contains("\"am\"")
        {
            return Step::Tool {
                id: c,
                name: "contacts",
                args: json!({}),
            };
        }
        if common::run_inputs(messages) == "go" && !common::answers(messages, None) {
            return common::keep("k1", "asker", "HELPER-TASK: ask");
        }
        Step::Text("NOTED")
    }));
    let llm = ScriptedLlmClient::new(vec![step; 40]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    common::until(|| heard(&llm, true, "ANSWER")).await;
    common::until(|| heard(&llm, false, "ACK-ANSWER")).await;
    assert!(
        !llm.recorded()
            .iter()
            .any(|r| format!("{:?}", r.messages).contains("NO-MATCH")),
        "a sender's name matched nothing in contacts"
    );
    for id in ["am", "hm"] {
        let result = llm
            .recorded()
            .iter()
            .find(|r| common::answers(&r.messages, Some(id)))
            .map(|r| common::tool_text(&r.messages, id))
            .unwrap();
        assert!(
            result.contains("\"status\":\"sent\"") && result.contains("\"accepted\""),
            "{id}: {result}"
        );
    }
    daemon.idle(&daemon.agent).await;
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
