//! Forks (`assign` with `context: "fork"`, which forks through the engine's
//! own `fork_off`) wired to their parents (the owner's decision of
//! 2026-11-16): the engine seats a fork with no edge, and the daemon wires
//! it to its parent once it is seated (`mobs::follow`); its first calls wait
//! for that edge. The engine tells the parent of the fork's first run's
//! end; once it recorded that outcome the daemon retires a single-use fork
//! (`keep: false`, the default), after a run that a message read before
//! the first one ended started, told as done. A kept fork (`keep: true`)
//! stays, and each later run of it is told as a kept helper's.

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

/// A single-use fork `kid` on `task`.
fn fork(task: &str) -> Step {
    common::fork("f1", "kid", task, false)
}

/// A kept fork `kid` on `task`.
fn kept_fork(task: &str) -> Step {
    common::fork("f1", "kid", task, true)
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

/// Whether `member` is seated in the workspace's mob.
async fn seated(connection: &rkat_tui::client::transport::Connection, member: &str) -> bool {
    let members = connection
        .call("mob/members", json!({"mob_id": "tui"}))
        .await
        .unwrap();
    members["members"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["agent_identity"] == member)
}

/// A fork and its parent while the fork works, each claim shown
/// positively:
///
/// - wired from its first call: the fork's first step is `peers`, which
///   lists its parent (its calls waited for the edge);
/// - duplex: the fork's message is read by the parent's model, the parent's
///   (sent while the fork's first run is in its model call) by the fork's;
/// - the parent's message to its fork is a peer message (no review);
/// - told and retired: the engine's outcome tells the first run's end;
///   when the message started a run after it, that run's end is told as an
///   `assign` helper's is ("finished its task … retired"), once; either way
///   the fork is retired after, and never told as "went idle".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_and_its_parent_talk_both_ways_while_it_works() {
    let ws = Workspace::new();
    let hold = Arc::new(tokio::sync::Notify::new());
    let step = {
        let hold = Arc::clone(&hold);
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
                    return Step::Gated(Arc::clone(&hold), Box::new(Step::Text("FORK-FIRST")));
                }
                return peers("fp1");
            }
            // The parent's steps are keyed on its whole conversation: the
            // fork's message and the engine's outcome may come in one run.
            let all = format!("{messages:?}");
            if all.contains("\"pp1\"") && !all.contains("\"pm1\"") {
                return send("pm1", peer_id(messages, "kid"), "PARENT-HELLO");
            }
            if all.contains("FORK-HELLO") && !all.contains("\"pp1\"") {
                return peers("pp1");
            }
            if all.contains("\"f1\"") {
                return Step::Text("NOTED");
            }
            fork("FORK-TASK: talk to your parent")
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 30]);
    let classifier = rig::ScriptedClassifier::allowing(Vec::new());
    let setup = rig::Setup::default().classifier(Arc::clone(&classifier) as _);
    let args = setup.command_line(&ws.root);
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
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
    // The parent reads the fork's message and answers it while the fork's
    // model call is held.
    common::until(|| heard(&llm, false, "FORK-HELLO")).await;
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| !r.user().contains("FORK-TASK") && answered(&r.messages, "pm1"))
    })
    .await;
    let registry = daemon.registry();
    assert_eq!(registry.endings().told(), 0);
    hold.notify_one();
    // The fork reads it, and is retired after.
    common::until(|| heard(&llm, true, "PARENT-HELLO")).await;
    common::until_async(async || !seated(&connection, "kid").await).await;
    common::until(|| outcome_heard(&llm)).await;
    let read_in = llm
        .recorded()
        .iter()
        .filter(|r| r.user().contains("FORK-TASK"))
        .map(|r| format!("{:?}", r.messages))
        .find(|m| m.contains("PARENT-HELLO"))
        .unwrap();
    if read_in.contains("FORK-FIRST") {
        // A run after its first: told as done, with that run's answer.
        common::until(|| heard(&llm, false, "Your helper `kid` finished its task")).await;
        let note = llm
            .recorded()
            .iter()
            .filter(|r| !r.user().contains("FORK-TASK"))
            .map(|r| format!("{:?}", r.messages))
            .find(|m| m.contains("Your helper `kid` finished its task"))
            .unwrap();
        assert!(
            note.contains("FORK-LATER") && note.contains("It is retired"),
            "{note}"
        );
        assert_eq!(registry.endings().told(), 1);
    } else {
        // Read in its first run: the engine's outcome carries the answer.
        assert!(heard(&llm, false, "FORK-LATER"));
        assert_eq!(registry.endings().told(), 0);
    }
    assert!(!heard(&llm, false, "Your helper `kid` went idle"));
    // The parent's message to its own fork is a peer message: not reviewed.
    let reviewed = classifier.seen();
    assert!(
        !reviewed
            .iter()
            .any(|(tool, args, _)| tool == "send_message"
                && args.to_string().contains("PARENT-HELLO")),
        "{reviewed:?}"
    );
    // The outcome's run (heard above) ends before the stop
    // (lukacf/meerkat#2374).
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}

/// A fork whose first run completed is retired once the engine recorded
/// its outcome for its parent (which then reaches the parent): it leaves
/// the roster and its parent's `peers`, nothing of ours tells the parent
/// more, and a message to it afterwards is refused saying it finished and
/// was retired. An agent alone in its workspace is told so, and that every
/// peer it has is a helper.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_fork_is_retired_once_its_outcome_is_recorded() {
    let ws = Workspace::new();
    let go = Arc::new(tokio::sync::Notify::new());
    let step = {
        let go = Arc::clone(&go);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            if common::user_texts(messages).concat().contains("FORK-TASK") {
                return Step::Gated(Arc::clone(&go), Box::new(Step::Text("FORK-FIRST")));
            }
            let all = format!("{messages:?}");
            let has = |id: &str| all.contains(&format!("\"{id}\""));
            let users = common::user_texts(messages).concat();
            if !has("f1") {
                return fork("FORK-TASK: look");
            }
            if users.contains("list") && !has("pp0") {
                return peers("pp0");
            }
            if users.contains("again") && !has("m1") {
                return send("m1", peer_id(messages, "kid"), "UPDATE-NOTE");
            }
            if users.contains("again") && !has("pp1") {
                return peers("pp1");
            }
            Step::Text("NOTED")
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let agent = daemon.agent.clone();
    common::until_async(async || wired_to(&connection, "kid").await.contains(&agent)).await;
    daemon.prompt(&connection, "list").await;
    // Seated while it works.
    assert!(seated(&connection, "kid").await);
    go.notify_one();
    common::until(|| outcome_heard(&llm)).await;
    common::until_async(async || !seated(&connection, "kid").await).await;
    daemon.idle(&agent).await;
    let registry = daemon.registry();
    assert_eq!(registry.endings().told(), 0, "the outcome is the news");
    daemon.prompt(&connection, "again").await;
    daemon.idle(&agent).await;
    let requests = llm.recorded();
    let parent = requests
        .iter()
        .rfind(|r| !r.user().contains("FORK-TASK"))
        .unwrap();
    let results = |id: &str| {
        parent
            .messages
            .iter()
            .find_map(|m| match m {
                Message::ToolResults { results, .. } => results
                    .iter()
                    .find(|r| r.tool_use_id == id)
                    .map(meerkat_core::types::ToolResult::text_content),
                _ => None,
            })
            .unwrap()
    };
    // Listed while it worked; gone from `peers` after.
    assert!(results("pp0").contains("tui/main/kid"));
    assert!(
        !results("pp1").contains("tui/main/kid"),
        "{}",
        results("pp1")
    );
    let refused = results("m1");
    assert!(
        refused.contains("helper `kid` finished its task and was retired"),
        "{refused}"
    );
    // The fork read nothing more.
    assert!(!heard(&llm, true, "UPDATE-NOTE"));
    // Alone in its workspace: told so.
    let all = format!("{:?}", parent.messages);
    assert!(
        all.contains("No other agent works in this workspace now"),
        "{all}"
    );
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
    // Its run ends only once the edge was seen (it is retired after).
    let done = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::clone(&done);
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if common::user_texts(messages).concat().contains("FORK-TASK") {
            if after_peers(messages) {
                return Step::Gated(Arc::clone(&gate), Box::new(Step::Text("FORK-DONE")));
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
    done.notify_one();
    common::until(|| outcome_heard(&llm)).await;
    daemon.idle(&agent).await;
    drop(connection);
    daemon.stop().await;
}

/// Whether the last message is the engine's outcome of a fork's first run
/// (not the `fork_off` call's own result, whose note names the same words).
fn tail_is_outcome(messages: &[Message]) -> bool {
    !matches!(messages.last(), Some(Message::ToolResults { .. }))
        && last(messages).contains("Background fork_off job")
}

/// Whether the parent's model was given a fork's outcome: the fork's first
/// run ended (and the engine delivered its end). Any message of the
/// request but the `fork_off` call's result counts: the outcome may come
/// in one run with a message of the fork.
fn outcome_heard(llm: &ScriptedLlmClient) -> bool {
    llm.recorded()
        .iter()
        .filter(|r| !r.user().contains("FORK-TASK"))
        .any(|r| {
            r.messages.iter().any(|m| {
                !matches!(m, Message::ToolResults { .. })
                    && format!("{m:?}").contains("Background fork_off job")
            })
        })
}

/// A helper's fork (helpers may nest: `helpers.max_depth = 2`) is seated
/// in the helper's own mob, the workspace's, and wired to the helper that
/// forked it, not to the agent; once its outcome is recorded for the
/// helper it is retired, as an agent's fork is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_s_fork_is_wired_to_the_helper() {
    let ws = Workspace::new();
    let go = Arc::new(tokio::sync::Notify::new());
    let done = Arc::new(tokio::sync::Notify::new());
    let step = {
        let go = Arc::clone(&go);
        let done = Arc::clone(&done);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let user = common::user_texts(messages).concat();
            if user.contains("FORK-TASK") {
                if after_peers(messages) {
                    return Step::Gated(Arc::clone(&done), Box::new(Step::Text("FORK-DONE")));
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
    done.notify_one();
    common::until_async(async || !seated(&connection, "kid").await).await;
    assert!(seated(&connection, "scout").await);
    drop(connection);
    daemon.stop().await;
}

/// A fork still in its first run when the daemon stops does not outlive
/// the stop: the engine's fork supervisor takes the run the stop cancelled
/// for a failed one and retires the fork (`meerkat-mob` 0.9.15
/// `fork_member_then_run_detached`, `retire_failed_fork_child`); the
/// shutdown interrupts that retirement and the next start finishes it.
/// The roster then no longer lists the fork, and the parent is told that
/// the job failed in a run of its own: at that start, or (most often)
/// only right after its next run. Before the stop, the
/// `fork_off` call's own result already names "Background fork_off job"
/// (how it will be told), so only the outcome notice itself says the
/// first run ended ([`outcome_heard`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_stopped_in_its_first_run_is_retired_and_its_parent_told() {
    let ws = Workspace::new();
    let first = ScriptedLlmClient::new(vec![
        Step::Dynamic(Arc::new(|messages: &[Message]| {
            if common::user_texts(messages).concat().contains("FORK-TASK") {
                return Step::Hang;
            }
            if matches!(messages.last(), Some(Message::ToolResults { .. })) {
                return Step::Text("PARENT-WAITS");
            }
            fork("FORK-TASK: wait")
        }));
        12
    ]);
    let daemon = Running::start(&ws.root, &first).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let agent = daemon.agent.clone();
    common::until_async(async || wired_to(&connection, "kid").await.contains(&agent)).await;
    // The fork's model call is held: its first run is under way.
    common::until(|| heard(&first, true, "FORK-TASK")).await;
    daemon.idle(&agent).await;
    assert!(heard(&first, false, "Background fork_off job"));
    assert!(!outcome_heard(&first), "the fork's first run has not ended");
    drop(connection);
    daemon.stop().await;

    let second = ScriptedLlmClient::new(vec![Step::Text("NOTED"); 12]);
    let daemon = Running::start(&ws.root, &second).await;
    let connection = daemon.connect_bound().await;
    // Its retirement is finished by this start: the roster lets it go.
    common::until_async(async || {
        let members = connection
            .call("mob/members", json!({"mob_id": "tui"}))
            .await
            .unwrap();
        !members["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["agent_identity"] == "kid")
    })
    .await;
    // The parent is told in a run of its own: at the start when the engine
    // wakes it, else (most often) right after its next run, here the
    // user's message.
    daemon.prompt(&connection, "hello").await;
    common::until(|| outcome_heard(&second)).await;
    let told = second
        .recorded()
        .iter()
        .map(|r| last(&r.messages))
        .find(|tail| tail.contains("Background fork_off job"))
        .unwrap();
    assert!(told.contains("finished (failed)"), "{told}");
    daemon.idle(&agent).await;
    drop(connection);
    daemon.stop().await;
}

/// The edge's life: a kept fork restored by the next start is wired to its
/// parent again (the roster names the edge, and the parent's message
/// reaches the fork's model), and its later runs are told as a kept
/// helper's; a fork retired leaves its parent's edges.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_fork_is_wired_and_a_retired_one_is_not() {
    let ws = Workspace::new();
    let first = ScriptedLlmClient::new(vec![
        Step::Dynamic(Arc::new(|messages: &[Message]| {
            if common::user_texts(messages).concat().contains("FORK-TASK") {
                return Step::Text("FORK-FIRST");
            }
            if tail_is_outcome(messages) {
                return Step::Text("OUTCOME-NOTED");
            }
            if matches!(messages.last(), Some(Message::ToolResults { .. })) {
                return Step::Text("PARENT-WAITS");
            }
            kept_fork("FORK-TASK: wait")
        }));
        12
    ]);
    let daemon = Running::start(&ws.root, &first).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let agent = daemon.agent.clone();
    common::until_async(async || wired_to(&connection, "kid").await.contains(&agent)).await;
    // The fork's first run ended (a fork still in it does not outlive the
    // stop: `a_fork_stopped_in_its_first_run_is_retired_and_its_parent_told`),
    // and the outcome's run ends before the stop (lukacf/meerkat#2374).
    common::until(|| outcome_heard(&first)).await;
    daemon.idle(&agent).await;
    // Kept: its outcome retires nothing (no retire of it was started), and
    // the call's answer said how it stays.
    assert!(seated(&connection, "kid").await);
    assert_eq!(daemon.registry().endings().retires(), 0);
    let started = first
        .recorded()
        .iter()
        .find(|r| answered(&r.messages, "f1"))
        .map(|r| common::tool_text(&r.messages, "f1"))
        .unwrap();
    assert!(
        started.contains("on a fork of this conversation") && started.contains("It is kept"),
        "{started}"
    );
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

/// A single-use fork whose outcome the engine recorded, but that the daemon
/// stopped before retiring it (its retire held here), is restored by the
/// next start and retired then: its outcome on record says it is done.
/// Nothing more is told of it (the outcome was the news, before the stop).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_single_use_fork_whose_outcome_was_recorded_is_retired() {
    let ws = Workspace::new();
    let first = ScriptedLlmClient::new(vec![
        Step::Dynamic(Arc::new(|messages: &[Message]| {
            if common::user_texts(messages).concat().contains("FORK-TASK") {
                return Step::Text("FORK-FIRST");
            }
            if tail_is_outcome(messages) {
                return Step::Text("OUTCOME-NOTED");
            }
            if matches!(messages.last(), Some(Message::ToolResults { .. })) {
                return Step::Text("PARENT-WAITS");
            }
            fork("FORK-TASK: answer")
        }));
        12
    ]);
    let daemon = Running::start(&ws.root, &first).await;
    // The retire after the outcome waits until the daemon stopped.
    let release = Arc::new(tokio::sync::watch::channel(false).0);
    let reached = Arc::new(tokio::sync::Notify::new());
    {
        let reached = Arc::clone(&reached);
        let release = Arc::clone(&release);
        daemon
            .registry()
            .endings()
            .before_retiring(Arc::new(move |_key| {
                let mut released = release.subscribe();
                let reached = Arc::clone(&reached);
                Box::pin(async move {
                    reached.notify_one();
                    let _ = released.wait_for(|go| *go).await;
                })
            }));
    }
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let agent = daemon.agent.clone();
    common::until(|| outcome_heard(&first)).await;
    // Its retire started (the outcome was recorded and it is idle) and is
    // held: it is still seated when the daemon stops.
    tokio::time::timeout(std::time::Duration::from_secs(30), reached.notified())
        .await
        .unwrap();
    assert!(seated(&connection, "kid").await);
    daemon.idle(&agent).await;
    drop(connection);
    daemon.stop().await;
    release.send_replace(true);

    let second = ScriptedLlmClient::new(vec![Step::Text("NOTED"); 12]);
    let daemon = Running::start(&ws.root, &second).await;
    let connection = daemon.connect_bound().await;
    common::until_async(async || !seated(&connection, "kid").await).await;
    assert_eq!(daemon.registry().endings().told(), 0);
    drop(connection);
    daemon.stop().await;
}
