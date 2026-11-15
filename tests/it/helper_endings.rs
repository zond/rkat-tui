//! How a helper ends and how its parent is told (the owner's decisions of
//! 2026-11-15): an interrupt of the agent leaves its helpers running and
//! their dialogs open; the user stops one helper on its own (the client's
//! `/helpers` and full view, or Stop on one of its dialogs); every ending
//! its parent's own call did not return is told to the parent once.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::{Client, Running};
use rkat_tui::approval::mode::PermissionMode;
use rkat_tui::client::transport::Connection;
use rkat_tui::mobs::HelperKey;
use rkat_tui::ui::{Effect, EscalationAnswer, Msg, TurnFailure};
use serde_json::json;

fn delegate(id: &'static str, member: &str, task: &str) -> Step {
    Step::Tool {
        id,
        name: "delegate",
        args: json!({
            "member_id": member,
            "task": task,
            "result_label": "result",
            "max_text_bytes": 4000,
        }),
    }
}

fn results(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m, Message::ToolResults { .. }))
        .count()
}

/// Whether a request of the agent (not of the helper, whose task names
/// `HELPER-TASK`) carries `text`.
fn agent_heard(llm: &ScriptedLlmClient, text: &str) -> bool {
    llm.recorded()
        .iter()
        .filter(|r| !r.user().contains("HELPER-TASK"))
        .any(|r| format!("{:?}", r.messages).contains(text))
}

async fn members(connection: &Connection, mob: &str) -> Vec<String> {
    let members = connection
        .call("mob/members", json!({"mob_id": mob}))
        .await
        .unwrap();
    members["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| member["agent_identity"].as_str().unwrap().to_string())
        .collect()
}

async fn until_empty(connection: &Connection, mob: &str) {
    common::until_async(async || members(connection, mob).await.is_empty()).await;
}

/// The client's `HelperEnded` for `label`: how, and who was told.
async fn ended(client: &mut Client, label: &str) -> (String, Option<String>) {
    let label = label.to_string();
    match client
        .saw(|m| matches!(m, Msg::HelperEnded { path, .. } if path.last() == Some(&label)))
        .await
    {
        Msg::HelperEnded { how, told, .. } => (how, told),
        _ => unreachable!(),
    }
}

fn write(id: &'static str, path: &str, content: &str) -> Step {
    Step::Tool {
        id,
        name: "write_file",
        args: json!({"path": path, "content": content}),
    }
}

/// The user stops a running helper on its own (`rkat/stop_helper`, from the
/// `/helpers` picker or its full view): the helper is retired, its agent's
/// run goes on (its `delegate` returns, and its next model call follows),
/// and the agent is told in its run, in plain words.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_a_helper_stops_it_alone_and_tells_its_running_agent() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let user = common::user_texts(messages).concat();
        if user.contains("HELPER-TASK") {
            return Step::Hang;
        }
        if format!("{messages:?}").contains("The user stopped your helper") {
            return Step::Text("noted the stop");
        }
        match results(messages) {
            0 => delegate("d1", "slow", "HELPER-TASK: never ends"),
            _ => Step::Text("went on"),
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| r.user().contains("HELPER-TASK"))
    })
    .await;
    client.send(Effect::StopHelper(vec!["slow".into()]));
    let (how, told) = ended(&mut client, "slow").await;
    assert_eq!(how, "stopped by you");
    assert!(told.is_some_and(|t| t.ends_with("was told")));
    let connection = daemon.connect().await;
    until_empty(&connection, &format!("implicit-{}", daemon.session)).await;
    // The agent's run went on after the stop, and the agent heard of it.
    common::until(|| agent_heard(&llm, "The user stopped your helper `slow`.")).await;
    assert!(agent_heard(
        &llm,
        "Its file changes stay on disk; its conversation is not continued."
    ));
    client.quit().await;
    daemon.stop().await;
}

/// A stop the client names wrongly is refused with what to do.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_a_helper_that_is_not_running_says_so() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("hi")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.agent().await;
    client.send(Effect::StopHelper(vec!["ghost".into()]));
    let notice = client
        .wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.contains("not stopped")))
        .await;
    let Msg::Notice { text, .. } = notice else {
        unreachable!()
    };
    assert!(
        text.contains("helper ghost was not stopped")
            && text.contains("/helpers lists the running ones"),
        "{text}"
    );
    client.quit().await;
    daemon.stop().await;
}

/// An interrupt of the agent leaves the dialog of its helper's call open:
/// the user answers it after the interrupt and the helper's write runs
/// (before, the interrupt answered it Stop and denied the write).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupt_leaves_a_helper_s_dialog_open() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let user = common::user_texts(messages).concat();
        if user.contains("HELPER-TASK") {
            return match results(messages) {
                0 => write("w1", "helper.txt", "FROM-HELPER"),
                _ => Step::Text("helper done"),
            };
        }
        match results(messages) {
            0 => delegate("d1", "scout", "HELPER-TASK: write helper.txt"),
            _ => Step::Text("after"),
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.set_mode(PermissionMode::Manual).await;
    client.send(Effect::StartTurn("go".into()));
    // Manual mode asks for the helper's write.
    let (prompt, id) = client.escalation().await;
    assert_eq!(
        (prompt.tool.as_str(), prompt.agent.as_deref()),
        ("write_file", Some("scout"))
    );
    client.send(Effect::Interrupt);
    assert_eq!(client.turn_finished().await, Err(TurnFailure::Interrupted));
    // The dialog is still open: answered now, the write runs.
    client.send(Effect::RespondEscalation(id, EscalationAnswer::AllowOnce));
    common::until(|| ws.root.join("helper.txt").exists()).await;
    assert_eq!(
        std::fs::read_to_string(ws.root.join("helper.txt")).unwrap(),
        "FROM-HELPER"
    );
    let (how, told) = ended(&mut client, "scout").await;
    assert_eq!(how, "finished");
    assert!(told.is_some_and(|t| t.contains("told with your next message")));
    client.quit().await;
    daemon.stop().await;
}

/// Stop on the dialog of a helper's call stops that helper only: its
/// agent's run goes on (its `delegate` returns, its next model call
/// follows) and it is told; the helper's write never runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_on_a_helper_s_dialog_stops_that_helper_only() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let user = common::user_texts(messages).concat();
        if user.contains("HELPER-TASK") {
            return match results(messages) {
                0 => write("w1", "helper.txt", "FROM-HELPER"),
                _ => Step::Text("helper done"),
            };
        }
        match results(messages) {
            0 => delegate("d1", "scout", "HELPER-TASK: write helper.txt"),
            _ => Step::Text("the agent went on"),
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.set_mode(PermissionMode::Manual).await;
    client.send(Effect::StartTurn("go".into()));
    let (prompt, id) = client.escalation().await;
    assert_eq!(prompt.agent.as_deref(), Some("scout"));
    client.send(Effect::RespondEscalation(id, EscalationAnswer::Stop));
    let (how, _) = ended(&mut client, "scout").await;
    assert_eq!(how, "stopped by you");
    let connection = daemon.connect().await;
    until_empty(&connection, &format!("implicit-{}", daemon.session)).await;
    // The agent was not interrupted: its turn ends with its own answer.
    assert_eq!(
        client.turn_finished().await.unwrap().text,
        "the agent went on"
    );
    common::until(|| agent_heard(&llm, "The user stopped your helper `scout`.")).await;
    assert!(!ws.root.join("helper.txt").exists());
    client.quit().await;
    daemon.stop().await;
}

/// A `delegate` that returns its helper's answer is the notification
/// itself: the helper's ending is settled without a note (the endings
/// record says so, and nothing was told).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegate_that_returns_its_answer_sends_no_note() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        delegate("d1", "scout", "HELPER-TASK: say hi"),
        Step::Text("hi"),
        Step::Text("parent done"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "parent done");
    let registry = daemon.registry();
    let key = HelperKey::new(
        rkat_tui::mobs::delegates::implicit_mob(&daemon.session),
        "scout".into(),
    );
    common::until(|| registry.endings().settled(&key)).await;
    assert_eq!(registry.endings().told(), 0);
    assert!(
        !client
            .seen
            .iter()
            .any(|m| matches!(m, Msg::HelperEnded { .. }))
    );
    assert!(!agent_heard(&llm, "Your helper `scout`"));
    client.quit().await;
    daemon.stop().await;
}

/// A helper that ends while its agent is idle, and not just interrupted
/// (the user sent a message since), starts a run for the agent with the
/// note: a run no user started, shown to the client as one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ending_wakes_an_idle_agent_the_user_did_not_just_interrupt() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let step = {
        let gate = Arc::clone(&gate);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let user = common::user_texts(messages).concat();
            if user.contains("HELPER-TASK") {
                return Step::Gated(Arc::clone(&gate), Box::new(Step::Text("LATE-ANSWER")));
            }
            if user.contains("LATE-ANSWER") {
                return Step::Text("read the late answer");
            }
            if user.contains("meanwhile") {
                return Step::Text("idle now");
            }
            delegate("d1", "slow", "HELPER-TASK: take time")
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| r.user().contains("HELPER-TASK"))
    })
    .await;
    client.send(Effect::Interrupt);
    assert_eq!(client.turn_finished().await, Err(TurnFailure::Interrupted));
    client.send(Effect::StartTurn("meanwhile".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "idle now");
    gate.notify_one();
    let (how, told) = ended(&mut client, "slow").await;
    assert_eq!(how, "finished");
    assert!(told.is_some_and(|t| t.ends_with("was told")));
    client
        .saw(|m| matches!(m, Msg::Triggered { label, .. } if label == "a helper ended"))
        .await;
    common::until(|| agent_heard(&llm, "LATE-ANSWER")).await;
    client.quit().await;
    daemon.stop().await;
}

/// The agent's message after the restart: its text as the model read it.
fn message_with(llm: &ScriptedLlmClient, words: &str) -> String {
    llm.recorded()
        .iter()
        .rev()
        .find_map(|r| {
            r.messages.iter().rev().find_map(|m| match m {
                Message::User(u) if u.text_content().contains(words) => Some(u.text_content()),
                _ => None,
            })
        })
        .unwrap_or_default()
}

/// An ending held for an interrupted agent survives the daemon's restart
/// (`held-notes.json` under the realm) and travels with the user's first
/// message after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_ending_survives_a_daemon_restart() {
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::Departure;
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let step = {
        let gate = Arc::clone(&gate);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let user = common::user_texts(messages).concat();
            if user.contains("HELPER-TASK") {
                return Step::Gated(Arc::clone(&gate), Box::new(Step::Text("HELPER-ANSWER")));
            }
            if user.contains("again") {
                return Step::Text("after the restart");
            }
            delegate("d1", "slow", "HELPER-TASK: look")
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    let name = client.agent().await;
    client.send(Effect::StartTurn("go".into()));
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| r.user().contains("HELPER-TASK"))
    })
    .await;
    client.send(Effect::Interrupt);
    assert_eq!(client.turn_finished().await, Err(TurnFailure::Interrupted));
    gate.notify_one();
    let (how, told) = ended(&mut client, "slow").await;
    assert_eq!(how, "finished");
    assert!(told.is_some_and(|t| t.contains("told with your next message")));
    let held = common::realm_of(&ws.root).join("held-notes.json");
    assert!(
        std::fs::read_to_string(&held)
            .unwrap()
            .contains("HELPER-ANSWER")
    );
    client.send(Effect::Leave(Departure::Detach));
    client.quit().await;
    daemon.stop().await;
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach_agent(&ws.root, AgentChoice::Agent { name }).await;
    client.send(Effect::StartTurn("again".into()));
    assert_eq!(
        client.turn_finished().await.unwrap().text,
        "after the restart"
    );
    let message = message_with(&llm, "again");
    assert!(
        message.contains("Your helper `slow` finished") && message.contains("HELPER-ANSWER"),
        "{message}"
    );
    assert!(
        !std::fs::read_to_string(&held)
            .unwrap()
            .contains("HELPER-ANSWER")
    );
    client.quit().await;
    daemon.stop().await;
}

/// A `delegate` helper still running when the daemon stops (its agent was
/// interrupted, so idle) is restored by the engine at the next start, which
/// runs it again: its model call after the restart shows it. Its call is
/// gone with the old daemon, so it is a cut-off helper: when it finishes,
/// its answer is held for its agent (no run is started for the agent) and
/// travels with the user's next message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_the_engine_restores_after_a_restart_answers_its_agent() {
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::Departure;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let ws = Workspace::new();
    let helper_calls = Arc::new(AtomicUsize::new(0));
    let restarted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let step = {
        let helper_calls = Arc::clone(&helper_calls);
        let restarted = Arc::clone(&restarted);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            let user = common::user_texts(messages).concat();
            if user.contains("HELPER-TASK") {
                helper_calls.fetch_add(1, Ordering::SeqCst);
                return if restarted.load(Ordering::SeqCst) {
                    Step::Text("RESTORED-ANSWER")
                } else {
                    Step::Hang
                };
            }
            if user.contains("again") {
                return Step::Text("after the restart");
            }
            delegate("d1", "slow", "HELPER-TASK: runs on")
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    let name = client.agent().await;
    client.send(Effect::StartTurn("go".into()));
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| r.user().contains("HELPER-TASK"))
    })
    .await;
    client.send(Effect::Interrupt);
    assert_eq!(client.turn_finished().await, Err(TurnFailure::Interrupted));
    client.send(Effect::Leave(Departure::Detach));
    client.quit().await;
    daemon.stop().await;
    restarted.store(true, Ordering::SeqCst);
    let before = helper_calls.load(Ordering::SeqCst);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect().await;
    // The helper runs again after the restart, finishes and is retired.
    common::until(|| helper_calls.load(Ordering::SeqCst) > before).await;
    until_empty(&connection, &format!("implicit-{}", daemon.session)).await;
    let registry = daemon.registry();
    let key = HelperKey::new(
        rkat_tui::mobs::delegates::implicit_mob(&daemon.session),
        "slow".into(),
    );
    common::until(|| registry.endings().settled(&key)).await;
    let agent_calls = || {
        llm.recorded()
            .iter()
            .filter(|r| !r.user().contains("HELPER-TASK"))
            .count()
    };
    assert_eq!(agent_calls(), 1, "no run was started for the agent");
    let mut client = Client::attach_agent(&ws.root, AgentChoice::Agent { name }).await;
    client.send(Effect::StartTurn("again".into()));
    assert_eq!(
        client.turn_finished().await.unwrap().text,
        "after the restart"
    );
    let message = message_with(&llm, "again");
    assert!(
        message.contains("Your helper `slow` finished") && message.contains("RESTORED-ANSWER"),
        "{message}"
    );
    client.quit().await;
    daemon.stop().await;
}

/// A `delegate` that really timed out keeps the orphan retirement and its
/// agent is told that the helper was ended (the helper's model call was
/// made first, so it was seated: 8 s leave a slow runner the time).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_timed_out_delegate_is_retired_and_its_agent_told() {
    let ws = Workspace::new();
    let config = common::realm_of(&ws.root).join("config.toml");
    std::fs::write(&config, "[tools.tool_timeouts]\ndelegate = \"8s\"\n").unwrap();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if common::user_texts(messages)
            .concat()
            .contains("HELPER-TASK")
        {
            return Step::Hang;
        }
        if format!("{messages:?}").contains("timed out. Its file changes") {
            return Step::Text("told");
        }
        match results(messages) {
            0 => delegate("d1", "stuck", "HELPER-TASK: never finishes"),
            _ => Step::Text("after the timeout"),
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| r.user().contains("HELPER-TASK"))
    })
    .await;
    let (how, _) = ended(&mut client, "stuck").await;
    assert_eq!(how, "timed out");
    let connection = daemon.connect().await;
    until_empty(&connection, &format!("implicit-{}", daemon.session)).await;
    common::until(|| {
        agent_heard(
            &llm,
            "Your helper `stuck` was ended because its `delegate` call timed out.",
        )
    })
    .await;
    client.quit().await;
    daemon.stop().await;
}
