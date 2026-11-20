//! How a helper ends and how its parent is told (the owner's decisions):
//! an interrupt of the agent leaves its helpers running and
//! their dialogs open; the user stops one helper on its own (the client's
//! `/helpers` and full view, or Stop on one of its dialogs); every ending
//! is told to the parent once. The helpers are `assign`'s: the agent's
//! run is held open explicitly (a gated or unanswered model call) where a
//! test needs it running while its helper runs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, after, assign, is_helper};
use meerkat_core::Message;
use rig::{Client, Running};
use rkat_tui::approval::mode::PermissionMode;
use rkat_tui::client::transport::Connection;
use rkat_tui::mobs::HelperKey;
use rkat_tui::ui::{Effect, EscalationAnswer, Msg, TurnFailure};
use serde_json::json;

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
        .filter(|r| !is_helper(&r.messages, "HELPER-TASK"))
        .any(|r| format!("{:?}", r.messages).contains(text))
}

/// The agent's model calls so far (not its helper's).
fn agent_calls(llm: &ScriptedLlmClient) -> usize {
    llm.recorded()
        .iter()
        .filter(|r| !is_helper(&r.messages, "HELPER-TASK"))
        .count()
}

/// Whether the helper `name` is seated in the workspace's mob.
async fn seated(connection: &Connection, name: &str) -> bool {
    members(connection, "tui").await.iter().any(|m| m == name)
}

/// Wait until the helper `name` left the workspace's mob.
async fn until_gone(connection: &Connection, name: &str) {
    common::until_async(async || !seated(connection, name).await).await;
}

/// A step that holds the agent's run until `gate` is notified, then calls
/// a tool that reads nothing (so the run makes one more model call).
fn held(gate: &Arc<tokio::sync::Notify>) -> Step {
    Step::Gated(
        Arc::clone(gate),
        Box::new(Step::Tool {
            id: "g1",
            name: "glob",
            args: json!({"pattern": "*.nothing"}),
        }),
    )
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

/// The client's `HelperEnded` for `label`: how, and who was told.
async fn ended(client: &mut Client, label: &str) -> (String, Option<String>) {
    let label = label.to_string();
    match client
        // How it ended, not the notice that it left its mob (which may come
        // first: its note goes after the endings' batch wait).
        .saw(|m| {
            matches!(m, Msg::HelperEnded { path, how, .. }
            if path.last() == Some(&label) && how != rkat_tui::daemon::protocol::HELPER_RETIRED)
        })
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
/// run (held in a model call meanwhile) goes on, and the agent is told in
/// plain words.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_a_helper_stops_it_alone_and_tells_its_running_agent() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let step = {
        let gate = Arc::clone(&gate);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            if is_helper(messages, "HELPER-TASK") {
                return Step::Hang;
            }
            if format!("{messages:?}").contains("The user stopped your helper") {
                return Step::Text("noted the stop");
            }
            if after(messages, "a1") {
                return held(&gate);
            }
            match results(messages) {
                0 => assign("a1", "slow", "HELPER-TASK: never ends"),
                _ => Step::Text("went on"),
            }
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| is_helper(&r.messages, "HELPER-TASK"))
    })
    .await;
    common::until(|| agent_calls(&llm) == 2).await;
    client.send(Effect::StopHelper(vec!["slow".into()]));
    let (how, told) = ended(&mut client, "slow").await;
    assert_eq!(how, "stopped by you");
    assert!(told.is_some_and(|t| t.ends_with("was told")));
    let connection = daemon.connect().await;
    until_gone(&connection, "slow").await;
    // The agent's run goes on after the stop, and the agent hears of it.
    gate.notify_one();
    assert_ne!(
        client.turn_finished().await,
        Err(TurnFailure::Interrupted),
        "the stop did not interrupt the agent"
    );
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
        if is_helper(messages, "HELPER-TASK") {
            return match results(messages) {
                0 => write("w1", "helper.txt", "FROM-HELPER"),
                _ => Step::Text("helper done"),
            };
        }
        match results(messages) {
            0 => assign("a1", "scout", "HELPER-TASK: write helper.txt"),
            // Held open for the interrupt.
            _ => Step::Hang,
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
    common::until(|| agent_calls(&llm) == 2).await;
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
/// agent's run (held in a model call meanwhile) goes on and it is told;
/// the helper's write never runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_on_a_helper_s_dialog_stops_that_helper_only() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let step = {
        let gate = Arc::clone(&gate);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            if is_helper(messages, "HELPER-TASK") {
                return match results(messages) {
                    0 => write("w1", "helper.txt", "FROM-HELPER"),
                    _ => Step::Text("helper done"),
                };
            }
            if after(messages, "a1") {
                return held(&gate);
            }
            match results(messages) {
                0 => assign("a1", "scout", "HELPER-TASK: write helper.txt"),
                _ => Step::Text("the agent went on"),
            }
        }))
    };
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
    until_gone(&connection, "scout").await;
    // The agent was not interrupted: its turn ends with its own answer.
    gate.notify_one();
    assert_eq!(
        client.turn_finished().await.unwrap().text,
        "the agent went on"
    );
    common::until(|| agent_heard(&llm, "The user stopped your helper `scout`.")).await;
    assert!(!ws.root.join("helper.txt").exists());
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
            if is_helper(messages, "HELPER-TASK") {
                return Step::Gated(Arc::clone(&gate), Box::new(Step::Text("LATE-ANSWER")));
            }
            let user = common::user_texts(messages).concat();
            if user.contains("LATE-ANSWER") {
                return Step::Text("read the late answer");
            }
            if user.contains("meanwhile") {
                return Step::Text("idle now");
            }
            match results(messages) {
                0 => assign("a1", "slow", "HELPER-TASK: take time"),
                // Held open for the interrupt.
                _ => Step::Hang,
            }
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| is_helper(&r.messages, "HELPER-TASK"))
    })
    .await;
    common::until(|| agent_calls(&llm) == 2).await;
    client.send(Effect::Interrupt);
    assert_eq!(client.turn_finished().await, Err(TurnFailure::Interrupted));
    client.send(Effect::StartTurn("meanwhile".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "idle now");
    gate.notify_one();
    let (how, told) = ended(&mut client, "slow").await;
    assert_eq!(how, "finished");
    assert!(told.is_some_and(|t| t.ends_with("was told")));
    // The run is announced with the note's folded line, made from the
    // ending itself.
    client
        .saw(|m| {
            matches!(m, Msg::Triggered { label, summary: Some(summary), .. }
                if label == "a helper ended"
                    && summary.title == "helper slow finished"
                    && summary.first == "LATE-ANSWER")
        })
        .await;
    common::until(|| agent_heard(&llm, "LATE-ANSWER")).await;
    client.quit().await;
    daemon.stop().await;
}

/// The script of the failed-steer tests: the agent starts `slow`, whose
/// answer waits for `gate`, and ends its run; a run that reads the note
/// says so, and so does the one that takes the user's `NEXT-ASK`.
fn failing_note_script(gate: &Arc<tokio::sync::Notify>) -> Arc<ScriptedLlmClient> {
    let gate = Arc::clone(gate);
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if is_helper(messages, "HELPER-TASK") {
            return Step::Gated(Arc::clone(&gate), Box::new(Step::Text("HELPER-ANSWER")));
        }
        let inputs = common::run_inputs(messages);
        if inputs.contains("NEXT-ASK") {
            return Step::Text("answered the next ask");
        }
        if inputs.contains("HELPER-ANSWER") {
            return Step::Text("read the note");
        }
        if common::answers(messages, Some("a1")) {
            return Step::Text("waiting for slow");
        }
        assign("a1", "slow", "HELPER-TASK: look")
    }));
    ScriptedLlmClient::new(vec![step; 20])
}

/// How many user messages of the agent's newest request carry `text`.
fn agent_carries(llm: &ScriptedLlmClient, text: &str) -> usize {
    llm.recorded()
        .iter()
        .rev()
        .find(|r| !is_helper(&r.messages, "HELPER-TASK"))
        .map_or(0, |r| {
            common::user_texts(&r.messages)
                .iter()
                .filter(|t| t.contains(text))
                .count()
        })
}

/// A helper's note whose steer into its idle agent the engine refuses is
/// not lost: it is held (durably), the client reads that the agent is told
/// with its next run and why, and it travels with the user's next message,
/// once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_whose_steer_fails_is_held_and_goes_with_the_next_message_once() {
    use rkat_tui::daemon::core::probe::NoteFault;
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = failing_note_script(&gate);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    let name = client.agent().await;
    client.send(Effect::StartTurn("go".into()));
    assert_eq!(
        client.turn_finished().await.unwrap().text,
        "waiting for slow"
    );
    daemon
        .local()
        .core()
        .probe
        .note_faults
        .lock()
        .push(NoteFault::Refused);
    gate.notify_one();
    let (how, told) = ended(&mut client, "slow").await;
    assert_eq!(how, "finished");
    assert_eq!(
        told.as_deref(),
        Some(
            format!(
                "{name} is told with its next run: the agent did not take it now (the note was refused on purpose (test))"
            )
            .as_str()
        )
    );
    let held = common::realm_of(&ws.root).join("held-notes.json");
    assert!(
        std::fs::read_to_string(&held)
            .unwrap()
            .contains("HELPER-ANSWER")
    );
    assert!(
        !agent_heard(&llm, "HELPER-ANSWER"),
        "the agent never read it"
    );
    client.send(Effect::StartTurn("NEXT-ASK".into()));
    assert_eq!(
        client.turn_finished().await.unwrap().text,
        "answered the next ask"
    );
    assert_eq!(agent_carries(&llm, "HELPER-ANSWER"), 1);
    assert_eq!(agent_carries(&llm, "NEXT-ASK"), 1);
    assert!(
        !std::fs::read_to_string(&held)
            .unwrap()
            .contains("HELPER-ANSWER")
    );
    client.quit().await;
    daemon.stop().await;
}

/// A steer the engine took but answered with an error is held as a failed
/// one is, and the run it started with the note's words shows it arrived:
/// the held copy goes, and the user's next message does not carry the note
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_steered_but_answered_with_an_error_is_not_told_twice() {
    use rkat_tui::daemon::core::probe::NoteFault;
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = failing_note_script(&gate);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    let name = client.agent().await;
    client.send(Effect::StartTurn("go".into()));
    assert_eq!(
        client.turn_finished().await.unwrap().text,
        "waiting for slow"
    );
    daemon
        .local()
        .core()
        .probe
        .note_faults
        .lock()
        .push(NoteFault::Late);
    gate.notify_one();
    let (_, told) = ended(&mut client, "slow").await;
    assert!(
        told.as_deref()
            .is_some_and(|t| t.starts_with(&format!("{name} is told with its next run: "))),
        "{told:?}"
    );
    // The note started a run of its own.
    common::until(|| agent_heard(&llm, "HELPER-ANSWER")).await;
    daemon.idle(&name).await;
    let held = common::realm_of(&ws.root).join("held-notes.json");
    common::until(|| {
        std::fs::read_to_string(&held).is_ok_and(|held| !held.contains("HELPER-ANSWER"))
    })
    .await;
    client.send(Effect::StartTurn("NEXT-ASK".into()));
    common::until(|| agent_carries(&llm, "NEXT-ASK") == 1).await;
    assert_eq!(agent_carries(&llm, "HELPER-ANSWER"), 1);
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
            if is_helper(messages, "HELPER-TASK") {
                return Step::Gated(Arc::clone(&gate), Box::new(Step::Text("HELPER-ANSWER")));
            }
            if common::user_texts(messages).concat().contains("again") {
                return Step::Text("after the restart");
            }
            match results(messages) {
                0 => assign("a1", "slow", "HELPER-TASK: look"),
                // Held open for the interrupt.
                _ => Step::Hang,
            }
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
            .any(|r| is_helper(&r.messages, "HELPER-TASK"))
    })
    .await;
    common::until(|| agent_calls(&llm) == 2).await;
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
        message.contains("Your helper `slow` finished its task")
            && message.contains("HELPER-ANSWER"),
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

/// An ending told within the delivery's batch window just before the
/// daemon stops (the window held open here until the stop) is not lost:
/// the stop holds it for the agent, durably (`held-notes.json`), and after
/// the next start it travels with the user's first message, once; the
/// message after that does not carry it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ending_told_just_before_a_stop_reaches_the_agent_after_the_start() {
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::Departure;
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if is_helper(messages, "HELPER-TASK") {
            return Step::Text("HELPER-ANSWER");
        }
        let user = common::user_texts(messages).concat();
        if user.contains("again") || user.contains("later") {
            return Step::Text("after the restart");
        }
        match results(messages) {
            0 => assign("a1", "quick", "HELPER-TASK: look"),
            _ => Step::Text("started it"),
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let daemon = Running::start(&ws.root, &llm).await;
    let registry = daemon.registry();
    registry.endings().hold_batches();
    let mut client = Client::attach(&ws.root).await;
    let name = client.agent().await;
    client.send(Effect::StartTurn("go".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "started it");
    // The helper finished and its ending was told to the delivery, which
    // holds it in its window.
    common::until(|| registry.endings().told() == 1).await;
    daemon.idle(&daemon.agent).await;
    assert!(!agent_heard(&llm, "Your helper `quick` finished"));
    client.send(Effect::Leave(Departure::Detach));
    client.quit().await;
    drop(registry);
    daemon.stop().await;
    let held = common::realm_of(&ws.root).join("held-notes.json");
    assert!(
        std::fs::read_to_string(&held)
            .unwrap()
            .contains("Your helper `quick` finished its task"),
        "held for the agent at the stop"
    );
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach_agent(&ws.root, AgentChoice::Agent { name }).await;
    client.send(Effect::StartTurn("again".into()));
    assert_eq!(
        client.turn_finished().await.unwrap().text,
        "after the restart"
    );
    let message = message_with(&llm, "again");
    assert!(
        message.contains("Your helper `quick` finished its task")
            && message.contains("HELPER-ANSWER"),
        "{message}"
    );
    client.send(Effect::StartTurn("later".into()));
    client.turn_finished().await.unwrap();
    let last = llm.recorded().last().unwrap().messages.clone();
    let told = common::user_texts(&last)
        .iter()
        .filter(|text| text.contains("Your helper `quick` finished its task"))
        .count();
    assert_eq!(told, 1, "told once");
    assert!(
        !std::fs::read_to_string(&held)
            .unwrap()
            .contains("HELPER-ANSWER")
    );
    client.quit().await;
    daemon.stop().await;
}

/// The script of the restart tests: a helper whose task names
/// `HELPER-TASK` hangs before the restart (`restarted` unset) and answers
/// `RESTORED-ANSWER` after it, counting its calls in `helper_calls`; the
/// agent answers "again" with "after the restart", assigns `slow` on its
/// first call (when `assigns`), and is held open otherwise.
fn restart_script(
    helper_calls: &Arc<std::sync::atomic::AtomicUsize>,
    restarted: &Arc<std::sync::atomic::AtomicBool>,
    assigns: bool,
) -> Arc<ScriptedLlmClient> {
    use std::sync::atomic::Ordering;
    let helper_calls = Arc::clone(helper_calls);
    let restarted = Arc::clone(restarted);
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if is_helper(messages, "HELPER-TASK") {
            helper_calls.fetch_add(1, Ordering::SeqCst);
            return if restarted.load(Ordering::SeqCst) {
                Step::Text("RESTORED-ANSWER")
            } else {
                Step::Hang
            };
        }
        if common::user_texts(messages).concat().contains("again") {
            return Step::Text("after the restart");
        }
        match results(messages) {
            0 if assigns => assign("a1", "slow", "HELPER-TASK: runs on"),
            _ => Step::Hang,
        }
    }));
    ScriptedLlmClient::new(vec![step; 20])
}

/// After the restart: the helper runs again, finishes and leaves `mob`
/// (`key` settled), no run is started for the agent (it made
/// `agent_calls_before` calls), and the user's next message carries its
/// answer after `note`.
#[allow(clippy::too_many_arguments)]
async fn restored_helper_answers(
    ws: &Workspace,
    llm: &Arc<ScriptedLlmClient>,
    name: String,
    helper_calls: &Arc<std::sync::atomic::AtomicUsize>,
    before: usize,
    agent_calls_before: usize,
    mob: &str,
    note: &str,
) {
    use rkat_tui::daemon::protocol::AgentChoice;
    use std::sync::atomic::Ordering;
    let daemon = Running::start(&ws.root, llm).await;
    let connection = daemon.connect().await;
    common::until(|| helper_calls.load(Ordering::SeqCst) > before).await;
    common::until_async(async || !members(&connection, mob).await.iter().any(|m| m == "slow"))
        .await;
    let registry = daemon.registry();
    let key = HelperKey::new(mob.into(), "slow".into());
    common::until(|| registry.endings().settled(&key)).await;
    assert_eq!(
        agent_calls(llm),
        agent_calls_before,
        "no run was started for the agent"
    );
    let mut client = Client::attach_agent(&ws.root, AgentChoice::Agent { name }).await;
    client.send(Effect::StartTurn("again".into()));
    assert_eq!(
        client.turn_finished().await.unwrap().text,
        "after the restart"
    );
    let message = message_with(llm, "again");
    assert!(
        message.contains(note) && message.contains("RESTORED-ANSWER"),
        "{message}"
    );
    client.quit().await;
    daemon.stop().await;
}

/// An `assign` helper still running when the daemon stops (its agent was
/// interrupted, so idle) is restored by the engine at the next start, which
/// runs it again: its model call after the restart shows it. When it
/// finishes, its answer is held for its agent (no run is started for the
/// agent) and travels with the user's next message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_assigned_helper_the_engine_restores_after_a_restart_answers_its_agent() {
    use rkat_tui::ui::Departure;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let ws = Workspace::new();
    let helper_calls = Arc::new(AtomicUsize::new(0));
    let restarted = Arc::new(AtomicBool::new(false));
    let llm = restart_script(&helper_calls, &restarted, true);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    let name = client.agent().await;
    client.send(Effect::StartTurn("go".into()));
    common::until(|| helper_calls.load(Ordering::SeqCst) > 0).await;
    common::until(|| agent_calls(&llm) == 2).await;
    client.send(Effect::Interrupt);
    assert_eq!(client.turn_finished().await, Err(TurnFailure::Interrupted));
    client.send(Effect::Leave(Departure::Detach));
    client.quit().await;
    daemon.stop().await;
    restarted.store(true, Ordering::SeqCst);
    let before = helper_calls.load(Ordering::SeqCst);
    restored_helper_answers(
        &ws,
        &llm,
        name,
        &helper_calls,
        before,
        2,
        "tui",
        "Your helper `slow` finished its task",
    )
    .await;
}

/// A realm an earlier build left with a `delegate` helper still running
/// (the engine's own tool, withdrawn since): the helper is a durable
/// member of its agent's implicit mob `implicit-<session id>`, seated by
/// the mob's `delegate` profile and bound to the agent's session. The
/// state is made here as that build's `delegate` made it, through the mob
/// host, since no agent is offered the tool any more. At the next start
/// the engine restores the helper and runs it again; its call ended with
/// the old daemon, so when it finishes its answer is held for its agent
/// (no run is started for it), it is retired, and the user's next message
/// carries the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegate_helper_an_earlier_build_left_finishes_and_answers_its_agent() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let ws = Workspace::new();
    let helper_calls = Arc::new(AtomicUsize::new(0));
    let restarted = Arc::new(AtomicBool::new(false));
    let llm = restart_script(&helper_calls, &restarted, false);
    let daemon = Running::start(&ws.root, &llm).await;
    let owner = daemon.session.clone();
    let name = daemon.agent.clone();
    let mob = format!("implicit-{owner}");
    let mut definition = meerkat_mob::MobDefinition::implicit(&owner.to_string(), common::MODEL);
    for profile in definition.profiles.values_mut() {
        if let meerkat_mob::ProfileBinding::Inline(profile) = profile {
            profile.durable = true;
        }
    }
    common::within(
        daemon
            .state()
            .mob_create_definition_with_owner_bridge_session(definition, owner, false, false),
    )
    .await
    .unwrap();
    let handle = common::within(daemon.state().handle_for(&mob.as_str().into()))
        .await
        .unwrap();
    let mut spec = meerkat_mob::SpawnMemberSpec::new("delegate", "slow");
    spec.initial_message = Some("HELPER-TASK: runs on".into());
    common::within(handle.spawn_spec(spec)).await.unwrap();
    common::until(|| helper_calls.load(Ordering::SeqCst) > 0).await;
    drop(handle);
    daemon.stop().await;
    restarted.store(true, Ordering::SeqCst);
    let before = helper_calls.load(Ordering::SeqCst);
    restored_helper_answers(
        &ws,
        &llm,
        name,
        &helper_calls,
        before,
        0,
        &mob,
        "Your helper `slow` finished.",
    )
    .await;
}

/// The script of the stop-and-retire tests: the helper answers
/// `HELPER-ANSWER` at once; the agent assigns `scout`, waits, and notes
/// the ending.
fn finishing_script() -> Arc<ScriptedLlmClient> {
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if is_helper(messages, "HELPER-TASK") {
            return Step::Text("HELPER-ANSWER");
        }
        if common::told_finished(messages) {
            return Step::Text("NOTED");
        }
        if after(messages, "a1") {
            return Step::Text("WAITING");
        }
        if results(messages) == 0 && !format!("{messages:?}").contains("finished its task") {
            return assign("a1", "scout", "HELPER-TASK: answer");
        }
        Step::Text("NOTED")
    }));
    ScriptedLlmClient::new(vec![step; 20])
}

/// A hook that tells `reached` and then waits for `release`.
fn holding(
    reached: &Arc<tokio::sync::Notify>,
    release: &Arc<tokio::sync::watch::Sender<bool>>,
) -> rkat_tui::mobs::endings::DecidingHookFn {
    let (reached, release) = (Arc::clone(reached), Arc::clone(release));
    Arc::new(move |_key| {
        let mut released = release.subscribe();
        let reached = Arc::clone(&reached);
        Box::pin(async move {
            reached.notify_one();
            let _ = released.wait_for(|go| *go).await;
        })
    })
}

/// How many notes in `session`'s transcript tell that `scout` finished.
async fn finished_notes(daemon: &Running, session: &meerkat_core::SessionId) -> usize {
    daemon
        .transcript(session)
        .await
        .iter()
        .filter(|m| !matches!(m, Message::ToolResults { .. }))
        .filter(|m| format!("{m:?}").contains("Your helper `scout` finished its task"))
        .count()
}

/// The daemon is stopped while the retire of a finished `assign` helper
/// is reaching the engine (held there): the stop waits for it before it
/// stops the mobs, so the stop is clean (the engine's stop of a member a
/// retire meets mid-way failed the runtime's shutdown, once on hosted CI),
/// and the next start finds the helper gone, its agent seated, and the
/// ending told once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_waits_for_a_retire_under_way_and_stops_cleanly() {
    let ws = Workspace::new();
    let llm = finishing_script();
    let daemon = Running::start(&ws.root, &llm).await;
    let registry = daemon.registry();
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::watch::channel(false).0);
    registry
        .endings()
        .retirements()
        .before_engine(holding(&reached, &release));
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    common::within(reached.notified()).await;
    // Its ending reached the agent (the retire comes after it).
    common::until(|| agent_heard(&llm, "Your helper `scout` finished its task")).await;
    daemon.idle(&daemon.agent).await;
    let session = daemon.current_session().await;
    drop(connection);
    let stopped = tokio::spawn(daemon.stop());
    // The stop closed the retirements and waits for the one held.
    common::until(|| registry.endings().retirements().closed()).await;
    assert!(!stopped.is_finished(), "the stop waits for the retire");
    release.send_replace(true);
    stopped.await.unwrap();

    let again = ScriptedLlmClient::new(vec![Step::Text("NOTED"); 12]);
    let daemon = Running::start(&ws.root, &again).await;
    let connection = daemon.connect_bound().await;
    let seated_now = members(&connection, "tui").await;
    assert!(seated_now.contains(&daemon.agent), "{seated_now:?}");
    assert!(!seated_now.iter().any(|m| m == "scout"), "{seated_now:?}");
    daemon.idle(&daemon.agent).await;
    assert_eq!(finished_notes(&daemon, &session).await, 1);
    assert_eq!(daemon.registry().endings().told(), 0);
    drop(connection);
    daemon.stop().await;
}

/// A finished `assign` helper whose retire had not reached the engine
/// when the daemon began to stop is not retired then (no retire starts
/// once the stop began): it is still seated at the next start, which
/// retires it, and its agent is not told of it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retire_the_stop_held_back_is_made_at_the_next_start() {
    let ws = Workspace::new();
    let llm = finishing_script();
    let daemon = Running::start(&ws.root, &llm).await;
    let registry = daemon.registry();
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::watch::channel(false).0);
    registry
        .endings()
        .before_retiring(holding(&reached, &release));
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    common::within(reached.notified()).await;
    common::until(|| agent_heard(&llm, "Your helper `scout` finished its task")).await;
    let session = daemon.current_session().await;
    assert!(seated(&connection, "scout").await);
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
    // Let go after the stop: it finds the retirements closed.
    release.send_replace(true);

    let again = ScriptedLlmClient::new(vec![Step::Text("NOTED"); 12]);
    let daemon = Running::start(&ws.root, &again).await;
    let connection = daemon.connect_bound().await;
    until_gone(&connection, "scout").await;
    assert!(members(&connection, "tui").await.contains(&daemon.agent));
    daemon.idle(&daemon.agent).await;
    assert_eq!(finished_notes(&daemon, &session).await, 1);
    assert_eq!(daemon.registry().endings().told(), 0);
    drop(connection);
    daemon.stop().await;
}

/// The text of the newest result of the call `id` the agent's model saw.
fn newest_result(llm: &ScriptedLlmClient, id: &str) -> Option<String> {
    llm.recorded().iter().rev().find_map(|r| {
        r.messages.iter().rev().find_map(|m| match m {
            Message::ToolResults { results, .. } => results
                .iter()
                .find(|res| res.tool_use_id == id)
                .map(meerkat_core::types::ToolResult::text_content),
            _ => None,
        })
    })
}

/// A `retire_helper` the engine keeps refusing answers that the helper is
/// still seated and to call again, and the helper is seated and owed (a
/// start retires it); called again once the engine lets it, it is
/// retired. A retire whose call is interrupted while it reaches the
/// engine still finishes: the helper leaves its mob.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_retire_is_answered_so_and_an_interrupted_one_still_finishes() {
    use meerkat_mob::{AgentIdentity, MobId};
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if is_helper(messages, "KEPT-TASK") {
            return Step::Text("KEPT-DONE");
        }
        if matches!(messages.last(), Some(Message::ToolResults { .. })) {
            return Step::Text("NOTED");
        }
        let user = common::run_inputs(messages);
        let retire = |id, name: &str| Step::Tool {
            id,
            name: "retire_helper",
            args: json!({"name": name}),
        };
        if user.contains("KEEP-TWO") {
            return Step::Tools(vec![
                (
                    "a-k1",
                    "assign",
                    json!({"name": "k1", "task": "KEPT-TASK one", "keep": true}),
                ),
                (
                    "a-k2",
                    "assign",
                    json!({"name": "k2", "task": "KEPT-TASK two", "keep": true}),
                ),
            ]);
        }
        if user.contains("RETIRE-K1") {
            return retire("r-k1", "k1");
        }
        if user.contains("RETIRE-K2") {
            return retire("r-k2", "k2");
        }
        Step::Text("NOTED")
    }));
    let llm = ScriptedLlmClient::new(vec![step; 40]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect().await;
    let mut client = Client::attach(&ws.root).await;
    let registry = daemon.registry();
    let k1 = HelperKey::new(MobId::from("tui"), AgentIdentity::from("k1"));
    client.send(Effect::StartTurn("KEEP-TWO".into()));
    client.turn_finished().await.unwrap();
    common::until(|| agent_heard(&llm, "KEPT-DONE")).await;
    assert!(seated(&connection, "k1").await && seated(&connection, "k2").await);
    daemon.idle(&daemon.agent).await;

    // Refused: the answer says so, the helper stays seated and owed.
    registry
        .endings()
        .retirements()
        .refuse(&k1, Some("REFUSED-BY-TEST"));
    client.send(Effect::StartTurn("RETIRE-K1".into()));
    client.turn_finished().await.unwrap();
    let refused = newest_result(&llm, "r-k1").unwrap();
    assert!(
        refused.contains(
            "Helper `k1` could not be retired (REFUSED-BY-TEST): it is still seated and `contacts` lists it. Call `retire_helper` for it again"
        ),
        "{refused}"
    );
    assert!(seated(&connection, "k1").await);
    assert!(registry.retire_owed(&k1));
    daemon.idle(&daemon.agent).await;
    // Let through: called again, it is retired.
    registry.endings().retirements().refuse(&k1, None);
    client.send(Effect::StartTurn("RETIRE-K1 again".into()));
    client.turn_finished().await.unwrap();
    assert_eq!(
        newest_result(&llm, "r-k1").as_deref(),
        Some("Helper `k1` was retired.")
    );
    until_gone(&connection, "k1").await;
    daemon.idle(&daemon.agent).await;

    // Held as it reaches the engine, its call interrupted: it finishes.
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::watch::channel(false).0);
    registry
        .endings()
        .retirements()
        .before_engine(holding(&reached, &release));
    client.send(Effect::StartTurn("RETIRE-K2".into()));
    common::within(reached.notified()).await;
    client.send(Effect::Interrupt);
    release.send_replace(true);
    assert_eq!(client.turn_finished().await, Err(TurnFailure::Interrupted));
    until_gone(&connection, "k2").await;
    client.quit().await;
    drop(connection);
    daemon.stop().await;
}
