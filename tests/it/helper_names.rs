//! Helper names: which names `assign` refuses, and what a name an ended
//! helper had starts. A name is a member id of the caller's mob (the
//! workspace's mob `tui` holds every agent and every helper, at every
//! level), so a name in use by any seated member is refused for every
//! kind; an agent's name is reserved. A name whose helper ended (a
//! single-use one finished, a kept one retired) starts a new helper of
//! any kind, which is told as its own. For a fork that is the engine's
//! outcome record, keyed by the fork's job and member, not by the name
//! alone: a restart neither retires a second fork from the first one's
//! record nor delivers an outcome twice.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::Running;
use rkat_tui::client::transport::Connection;
use serde_json::json;

use crate::assign::peer_id;

/// The four kinds: `(context, keep)`.
const KINDS: [(&str, bool); 4] = [
    ("fresh", false),
    ("fresh", true),
    ("fork", false),
    ("fork", true),
];

/// Whether `messages` hold the tool call `id`.
fn has(messages: &[Message], id: &str) -> bool {
    format!("{messages:?}").contains(&format!("\"{id}\""))
}

/// The members seated in the workspace's mob.
async fn members(connection: &Connection) -> Vec<String> {
    let members = connection
        .call("mob/members", json!({"mob_id": "tui"}))
        .await
        .unwrap();
    members["members"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["agent_identity"].as_str().map(str::to_string))
        .collect()
}

async fn seated(connection: &Connection, member: &str) -> bool {
    members(connection).await.iter().any(|m| m == member)
}

/// The engine's fork outcomes in `messages`, in order ("Background
/// fork_off job … finished"; not the `assign` call's own answer, which
/// names the same words).
fn outcomes(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| !matches!(m, Message::ToolResults { .. }))
        .map(|m| format!("{m:?}"))
        .filter(|m| m.contains("Background fork_off job"))
        .collect()
}

/// The user texts of `messages` that carry `needle`.
fn notes(messages: &[Message], needle: &str) -> Vec<String> {
    common::user_texts(messages)
        .into_iter()
        .filter(|text| text.contains(needle))
        .collect()
}

/// The result of the call `id` in `session`'s transcript, once committed.
async fn result_of(daemon: &Running, session: &meerkat_core::SessionId, id: &str) -> String {
    common::poll(async || {
        daemon
            .transcript(session)
            .await
            .iter()
            .find_map(|m| match m {
                Message::ToolResults { results, .. } => results
                    .iter()
                    .find(|r| r.tool_use_id == id)
                    .map(meerkat_core::types::ToolResult::text_content),
                _ => None,
            })
    })
    .await
}

/// A connection bound to a new agent of the daemon, initialized.
async fn new_agent(daemon: &Running) -> Connection {
    let (connection, _notes) = Connection::attach_to(
        &daemon.endpoint(),
        std::time::Duration::from_secs(5),
        None,
        false,
        Some(rkat_tui::daemon::protocol::AgentChoice::New),
    )
    .await
    .unwrap();
    connection.call("initialize", json!({})).await.unwrap();
    connection
}

/// A name in use is refused for each of the four kinds, whoever holds it:
/// the caller's own kept helper seated idle, its own helper running, a
/// helper asking for a name its parent's helper has (another level of the
/// tree), another agent asking for the first agent's helpers' names; an
/// agent's name is reserved. Each refusal says what to do, and nothing is
/// seated for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_in_use_or_an_agent_s_is_refused_for_every_kind() {
    let ws = Workspace::new();
    let agent = Arc::new(parking_lot::Mutex::new(String::new()));
    let step = {
        let agent = Arc::clone(&agent);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            if common::is_helper(messages, "KEPT-TASK") {
                return Step::Text("K-DONE");
            }
            if common::is_helper(messages, "RUN-TASK") {
                return Step::Hang;
            }
            if common::is_helper(messages, "NEST-TASK") {
                if !has(messages, "n1") {
                    return Step::Tools(vec![
                        ("n1", "assign", json!({"name": "r", "task": "x"})),
                        (
                            "n2",
                            "assign",
                            json!({"name": "k", "task": "x", "context": "fork"}),
                        ),
                    ]);
                }
                return Step::Text("NEST-DONE");
            }
            let users = common::user_texts(messages).concat();
            if users.contains("PROMPT-OTHER") {
                if !has(messages, "o1") {
                    return Step::Tools(vec![
                        ("o1", "assign", json!({"name": "r", "task": "x"})),
                        (
                            "o2",
                            "assign",
                            json!({"name": "k", "task": "x", "context": "fork", "keep": true}),
                        ),
                    ]);
                }
                return Step::Text("OTHER-DONE");
            }
            if users.contains("PROMPT-NAMES") && !has(messages, "k0") {
                let mut calls = Vec::new();
                for (i, (context, keep)) in KINDS.iter().enumerate() {
                    for name in ["k", "r"] {
                        calls.push((
                            common::leak(format!("{name}{i}")),
                            "assign",
                            json!({"name": name, "task": "x", "context": context, "keep": keep}),
                        ));
                    }
                }
                calls.push((
                    "me",
                    "assign",
                    json!({"name": agent.lock().clone(), "task": "x", "context": "fork"}),
                ));
                return Step::Tools(calls);
            }
            if users.contains("PROMPT-START") && !has(messages, "a1") {
                return Step::Tools(vec![
                    (
                        "a1",
                        "assign",
                        json!({"name": "k", "task": "KEPT-TASK", "keep": true}),
                    ),
                    ("a2", "assign", json!({"name": "r", "task": "RUN-TASK"})),
                    ("a3", "assign", json!({"name": "h", "task": "NEST-TASK"})),
                ]);
            }
            Step::Text("NOTED")
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 40]);
    let setup = rig::Setup::args(&["--set", "helpers.max_depth=2"]);
    let args = setup.command_line(&ws.root);
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    agent.lock().clone_from(&daemon.agent);
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "PROMPT-START").await;
    // `k` is seated idle (its parent was told), `h` finished after its own
    // two calls, `r` runs on.
    common::until(|| {
        llm.recorded().iter().any(|r| {
            !common::is_helper(&r.messages, "TASK")
                && notes(&r.messages, "Your helper `k` went idle").len() == 1
        })
    })
    .await;
    common::until(|| common::parent_told(&llm, "NEST-TASK", "h")).await;
    daemon.idle(&daemon.agent).await;
    assert!(seated(&connection, "k").await && seated(&connection, "r").await);
    daemon.prompt(&connection, "PROMPT-NAMES").await;
    let own = daemon.current_session().await;
    for (i, (context, keep)) in KINDS.iter().enumerate() {
        for name in ["k", "r"] {
            let text = result_of(&daemon, &own, &format!("{name}{i}")).await;
            assert_eq!(
                text,
                format!(
                    "the name `{name}` is in use by a member seated now (`contacts` lists your helpers); call `assign` again with another name"
                ),
                "{name} as {context} keep {keep}"
            );
        }
    }
    let me = result_of(&daemon, &own, "me").await;
    assert_eq!(
        me,
        format!(
            "`{}` is reserved for the workspace's agents (`main-` and four characters); call `assign` again with another name",
            daemon.agent
        )
    );
    // Another level of the tree: `h`'s calls.
    let nested = llm
        .recorded()
        .into_iter()
        .find(|r| common::is_helper(&r.messages, "NEST-TASK") && has(&r.messages, "n1"))
        .map(|r| r.messages)
        .unwrap();
    for (id, name) in [("n1", "r"), ("n2", "k")] {
        let text = common::tool_text(&nested, id);
        assert!(
            text.starts_with(&format!("the name `{name}` is in use")),
            "{text}"
        );
    }
    // Another agent of the workspace.
    let other = new_agent(&daemon).await;
    assert_ne!(other.agent(), daemon.agent);
    daemon.prompt(&other, "PROMPT-OTHER").await;
    // Its run with the calls may come after one its joining started.
    let theirs = daemon.session_of(other.agent()).await;
    for (id, name) in [("o1", "r"), ("o2", "k")] {
        let text = result_of(&daemon, &theirs, id).await;
        assert!(
            text.starts_with(&format!("the name `{name}` is in use")),
            "{text}"
        );
    }
    // Nothing more was seated: the two agents and `k`, `r`.
    let mut seated_now = members(&connection).await;
    seated_now.sort();
    let mut expected = vec![
        daemon.agent.clone(),
        other.agent().to_string(),
        "k".to_string(),
        "r".to_string(),
    ];
    expected.sort();
    assert_eq!(seated_now, expected);
    daemon.idle(other.agent()).await;
    drop(other);
    drop(connection);
    daemon.stop().await;
}

/// The name of a helper that ended starts a new helper, of any kind: a
/// finished single-use fresh helper's name a kept fresh one, a kept fresh
/// helper's (retired with `retire_helper`) a single-use fork, a kept
/// fork's (retired) a single-use fresh helper. Each new helper runs its
/// own task and its parent is told of it as its own, once, after the
/// earlier one's news; a message to the new one reaches it (not refused
/// as the earlier one's retirement).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ended_helper_s_name_starts_a_new_helper_of_any_kind() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let users = common::user_texts(messages).concat();
        // Forks first: a fork's conversation is its parent's.
        if users.contains("FORK-P-1") {
            return Step::Text("P-ONE");
        }
        if users.contains("FORK-K-2") {
            return Step::Text("K-TWO");
        }
        if common::is_helper(messages, "SINGLE-S-1") {
            return Step::Text("S-ONE");
        }
        if common::is_helper(messages, "KEPT-K-1") {
            return Step::Text("K-ONE");
        }
        if common::is_helper(messages, "KEPT-S-2") {
            // The parent's message, in whatever form it is delivered.
            if format!("{messages:?}").contains("PING") {
                return Step::Text("S-PONG");
            }
            return Step::Text("S-TWO");
        }
        if common::is_helper(messages, "SINGLE-P-2") {
            return Step::Text("P-TWO");
        }
        if users.contains("PROMPT-PING") {
            if !has(messages, "m0") {
                return Step::Tool {
                    id: "m0",
                    name: "contacts",
                    args: json!({}),
                };
            }
            if !has(messages, "m1") {
                return Step::Tool {
                    id: "m1",
                    name: "send_message",
                    args: json!({
                        "peer_id": peer_id(messages, "main/s").unwrap_or_default(),
                        "body": "PING",
                        "handling_mode": "queue"
                    }),
                };
            }
        }
        if users.contains("PROMPT-AGAIN") && !has(messages, "b1") {
            return Step::Tools(vec![
                (
                    "b1",
                    "assign",
                    json!({"name": "s", "task": "KEPT-S-2", "keep": true}),
                ),
                (
                    "b2",
                    "assign",
                    json!({"name": "k", "task": "FORK-K-2", "context": "fork"}),
                ),
                ("b3", "assign", json!({"name": "p", "task": "SINGLE-P-2"})),
            ]);
        }
        if users.contains("PROMPT-RETIRE") && !has(messages, "r1") {
            return Step::Tools(vec![
                ("r1", "retire_helper", json!({"name": "k"})),
                ("r2", "retire_helper", json!({"name": "p"})),
            ]);
        }
        if users.contains("PROMPT-START") && !has(messages, "a1") {
            return Step::Tools(vec![
                ("a1", "assign", json!({"name": "s", "task": "SINGLE-S-1"})),
                (
                    "a2",
                    "assign",
                    json!({"name": "k", "task": "KEPT-K-1", "keep": true}),
                ),
                (
                    "a3",
                    "assign",
                    json!({"name": "p", "task": "FORK-P-1", "context": "fork", "keep": true}),
                ),
            ]);
        }
        Step::Text("NOTED")
    }));
    let llm = ScriptedLlmClient::new(vec![step; 60]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    let session = daemon.current_session().await;
    let transcript = async || daemon.transcript(&session).await;
    daemon.prompt(&connection, "PROMPT-START").await;
    common::until_async(async || {
        let messages = transcript().await;
        notes(&messages, "Your helper `s` finished its task").len() == 1
            && notes(&messages, "Your helper `k` went idle").len() == 1
            && outcomes(&messages).len() == 1
    })
    .await;
    common::until_async(async || !seated(&connection, "s").await).await;
    daemon.idle(&daemon.agent).await;
    daemon.prompt(&connection, "PROMPT-RETIRE").await;
    common::until_async(async || {
        !seated(&connection, "k").await && !seated(&connection, "p").await
    })
    .await;
    daemon.idle(&daemon.agent).await;
    daemon.prompt(&connection, "PROMPT-AGAIN").await;
    for id in ["b1", "b2", "b3"] {
        let text = result_of(&daemon, &session, id).await;
        assert!(text.contains("is running"), "{id}: {text}");
    }
    common::until_async(async || {
        let messages = transcript().await;
        notes(&messages, "Your helper `s` went idle").len() == 1
            && notes(&messages, "Your helper `p` finished its task").len() == 1
            && outcomes(&messages).len() == 2
    })
    .await;
    common::until_async(async || !seated(&connection, "k").await).await;
    daemon.idle(&daemon.agent).await;
    daemon.prompt(&connection, "PROMPT-PING").await;
    common::until_async(async || notes(&transcript().await, "S-PONG").len() == 1).await;
    daemon.idle(&daemon.agent).await;
    let messages = transcript().await;
    // Each helper told once, with its own answer, the earlier one's first.
    let s_done = notes(&messages, "Your helper `s` finished its task");
    assert!(
        s_done.len() == 1 && s_done[0].contains("S-ONE"),
        "{s_done:?}"
    );
    let s_idle = notes(&messages, "Your helper `s` went idle");
    assert_eq!(s_idle.len(), 2, "{s_idle:?}");
    assert!(s_idle[0].contains("S-TWO") && s_idle[1].contains("S-PONG"));
    let k_idle = notes(&messages, "Your helper `k` went idle");
    assert!(
        k_idle.len() == 1 && k_idle[0].contains("K-ONE"),
        "{k_idle:?}"
    );
    assert!(notes(&messages, "Your helper `k` finished").is_empty());
    let p_done = notes(&messages, "Your helper `p` finished its task");
    assert!(
        p_done.len() == 1 && p_done[0].contains("P-TWO"),
        "{p_done:?}"
    );
    let forks = outcomes(&messages);
    assert_eq!(forks.len(), 2, "{forks:?}");
    assert!(forks[0].contains("P-ONE") && !forks[0].contains("K-TWO"));
    assert!(forks[1].contains("K-TWO") && !forks[1].contains("P-ONE"));
    // The message to the new `s` reached it.
    let sent = common::tool_text(&messages, "m1");
    assert!(!sent.contains("retired"), "{sent}");
    assert!(seated(&connection, "s").await);
    // A new helper, not the earlier one's conversation picked up again:
    // the second `s` and `p` hold nothing of their earlier namesakes.
    for (task, earlier) in [("KEPT-S-2", "SINGLE-S-1"), ("SINGLE-P-2", "FORK-P-1")] {
        let theirs: Vec<String> = llm
            .recorded()
            .iter()
            .filter(|r| common::is_helper(&r.messages, task))
            .map(|r| format!("{:?}", r.messages))
            .collect();
        assert!(!theirs.is_empty(), "{task}");
        assert!(
            theirs.iter().all(|m| !m.contains(earlier)),
            "{task}: {theirs:?}"
        );
    }
    drop(connection);
    daemon.stop().await;
}

/// The step of a parent that forks `x` (single-use) on its first prompt and
/// again on `PROMPT-AGAIN`; the first fork answers `FORK-ONE`, the second
/// takes `second`.
fn forking_twice(second: Step) -> Step {
    Step::Dynamic(Arc::new(move |messages: &[Message]| {
        let users = common::user_texts(messages).concat();
        if users.contains("FORK-TASK-1") {
            return Step::Text("FORK-ONE");
        }
        if users.contains("FORK-TASK-2") {
            return second.clone();
        }
        if !has(messages, "f1") {
            return common::fork("f1", "x", "FORK-TASK-1: answer one", false);
        }
        if users.contains("PROMPT-AGAIN") && !has(messages, "f2") {
            return common::fork("f2", "x", "FORK-TASK-2: answer two", false);
        }
        Step::Text("NOTED")
    }))
}

/// The helper `x` of the workspace's mob.
fn x() -> rkat_tui::mobs::HelperKey {
    rkat_tui::mobs::HelperKey::new(
        meerkat_mob::MobId::from("tui"),
        meerkat_mob::AgentIdentity::from("x"),
    )
}

/// The first fork `x` ran, was told by its outcome and retired (its
/// binding gone too).
async fn first_fork_done(daemon: &Running, connection: &Connection) {
    daemon.prompt(connection, "PROMPT-START").await;
    let session = daemon.current_session().await;
    common::until_async(async || outcomes(&daemon.transcript(&session).await).len() == 1).await;
    common::until_async(async || !seated(connection, "x").await).await;
    let registry = daemon.registry();
    common::until(|| registry.member_session(&x()).is_none()).await;
    assert_eq!(registry.endings().outcomes_reported(), 1);
    daemon.idle(&daemon.agent).await;
}

/// The second fork `x` runs its first run and is followed: its follower
/// looked for its outcome on record before it bound the fork's session,
/// and did not take the first's for it.
async fn second_fork_followed(daemon: &Running, llm: &ScriptedLlmClient, connection: &Connection) {
    common::until(|| {
        llm.recorded().iter().any(|r| {
            common::user_texts(&r.messages)
                .concat()
                .contains("FORK-TASK-2")
        })
    })
    .await;
    let registry = daemon.registry();
    common::until(|| registry.member_session(&x()).is_some()).await;
    assert_eq!(
        registry.endings().outcomes_reported(),
        1,
        "the first `x`'s outcome was taken for the second's"
    );
    assert!(seated(connection, "x").await);
}

/// A single-use fork's name used again once it finished: the second `x` is
/// seated, runs its own task and is told by its own outcome; the parent
/// holds exactly two outcomes, the first's then the second's, and each
/// fork was retired once. Stopped after the second finished, the next
/// start delivers neither again and retires nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_s_name_used_again_is_told_by_its_own_outcome_also_after_a_restart() {
    let ws = Workspace::new();
    let go = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        forking_twice(Step::Gated(
            Arc::clone(&go),
            Box::new(Step::Text("FORK-TWO"))
        ));
        30
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    first_fork_done(&daemon, &connection).await;
    daemon.prompt(&connection, "PROMPT-AGAIN").await;
    let session = daemon.current_session().await;
    let started = result_of(&daemon, &session, "f2").await;
    assert!(
        started.contains("Helper `x` is running on a fork"),
        "{started}"
    );
    // Seated while it works, the first `x`'s record notwithstanding.
    second_fork_followed(&daemon, &llm, &connection).await;
    go.notify_one();
    common::until_async(async || outcomes(&daemon.transcript(&session).await).len() == 2).await;
    common::until_async(async || !seated(&connection, "x").await).await;
    // The outcome's run ends before the stop (lukacf/meerkat#2374).
    daemon.idle(&daemon.agent).await;
    let told = outcomes(&daemon.transcript(&session).await);
    assert!(
        told[0].contains("FORK-ONE") && !told[0].contains("FORK-TWO"),
        "{told:?}"
    );
    assert!(
        told[1].contains("FORK-TWO") && !told[1].contains("FORK-ONE"),
        "{told:?}"
    );
    let registry = daemon.registry();
    assert_eq!(registry.endings().retires(), 2);
    assert_eq!(registry.endings().outcomes_reported(), 2, "each fork's own");
    assert_eq!(registry.endings().told(), 0, "the outcomes are the news");
    drop(connection);
    daemon.stop().await;

    let again = ScriptedLlmClient::new(vec![Step::Text("NOTED"); 12]);
    let daemon = Running::start(&ws.root, &again).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "hello").await;
    daemon.idle(&daemon.agent).await;
    let told_after = outcomes(&daemon.transcript(&session).await);
    assert_eq!(told_after, told, "nothing delivered again");
    assert!(!seated(&connection, "x").await);
    assert_eq!(daemon.registry().endings().retires(), 0);
    drop(connection);
    daemon.stop().await;
}

/// The second fork `x` still in its first run when the daemon stops, the
/// first `x`'s completed outcome on record: the next start does not take
/// that record for the second's (the record names the first's job). The
/// engine retires the second as failed (lukacf/meerkat#2384) and tells the
/// parent so in an outcome of its own; the daemon retires nothing itself,
/// and the first's outcome is not delivered again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_does_not_take_the_first_fork_s_outcome_for_the_second_s() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![forking_twice(Step::Hang); 30]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    first_fork_done(&daemon, &connection).await;
    daemon.prompt(&connection, "PROMPT-AGAIN").await;
    second_fork_followed(&daemon, &llm, &connection).await;
    daemon.idle(&daemon.agent).await;
    assert_eq!(daemon.registry().endings().retires(), 1);
    let session = daemon.current_session().await;
    assert_eq!(outcomes(&daemon.transcript(&session).await).len(), 1);
    drop(connection);
    daemon.stop().await;

    let again = ScriptedLlmClient::new(vec![Step::Text("NOTED"); 12]);
    let daemon = Running::start(&ws.root, &again).await;
    let connection = daemon.connect_bound().await;
    common::until_async(async || !seated(&connection, "x").await).await;
    daemon.prompt(&connection, "hello").await;
    common::until_async(async || outcomes(&daemon.transcript(&session).await).len() == 2).await;
    daemon.idle(&daemon.agent).await;
    let told = outcomes(&daemon.transcript(&session).await);
    assert_eq!(told.len(), 2, "{told:?}");
    assert!(told[0].contains("FORK-ONE"), "{told:?}");
    assert!(
        told[1].contains("finished (failed)") && !told[1].contains("FORK-ONE"),
        "{told:?}"
    );
    assert_eq!(
        daemon.registry().endings().retires(),
        0,
        "the second `x` was not retired from the first's record"
    );
    assert_eq!(daemon.registry().endings().outcomes_reported(), 0);
    drop(connection);
    daemon.stop().await;
}
