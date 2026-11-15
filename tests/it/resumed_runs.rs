//! A run the daemon's stop caught in the middle of a tool call. The next
//! daemon's engine resumes it (`run_resumed`, in place of `run_started`):
//! the same run goes on under its id and dispatches the round's calls
//! again. The agent is busy, its client joins the run, a message the user
//! types waits for it as any steer does, and Esc stops that run by its id
//! at once.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::{AgentEvent, Message};
use rig::{Client, Running};
use rkat_tui::client::transport::Connection;
use rkat_tui::daemon::protocol::AgentChoice;
use rkat_tui::daemon::protocol::engine::{FORCE_CANCEL, HARD_CANCEL_MEMBER, STOP_MEMBER_RUN};
use rkat_tui::ui::{Departure, Effect, Msg, PendingId, TurnFailure};
use serde_json::json;

fn results(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m, Message::ToolResults { .. }))
        .count()
}

/// Whether a request of the agent (not of its helper, whose task names
/// `HELPER-TASK`) carries `text` as a user's words.
fn agent_heard(llm: &ScriptedLlmClient, text: &str) -> bool {
    llm.recorded()
        .iter()
        .filter(|r| !r.user().contains("HELPER-TASK"))
        .any(|r| common::user_texts(&r.messages).iter().any(|t| t == text))
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

/// The agent's script: `call` first, `went on` once a result is in, an
/// answer for each later message; a helper's task hangs.
fn script(call: Step) -> Arc<ScriptedLlmClient> {
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        let user = common::user_texts(messages);
        if user.concat().contains("HELPER-TASK") {
            return Step::Hang;
        }
        if user.last().is_some_and(|last| last == "after the stop") {
            return Step::Text("answered after the stop");
        }
        if user.last().is_some_and(|last| last == "typed meanwhile") {
            return Step::Text("read the message");
        }
        match results(messages) {
            0 => call.clone(),
            _ => Step::Text("went on"),
        }
    }));
    ScriptedLlmClient::new(vec![step; 20])
}

fn delegate() -> Step {
    Step::Tool {
        id: "d1",
        name: "delegate",
        args: json!({
            "member_id": "slow",
            "task": "HELPER-TASK: runs on",
            "result_label": "result",
            "max_text_bytes": 4000,
        }),
    }
}

/// Start the agent's turn, wait for `inside` (its run is in the call), and
/// stop the daemon with the run open; the agent's name.
async fn stop_inside(
    ws: &Workspace,
    llm: &Arc<ScriptedLlmClient>,
    inside: impl FnMut() -> bool,
) -> String {
    let daemon = Running::start(&ws.root, llm).await;
    let mut client = Client::attach(&ws.root).await;
    let name = client.agent().await;
    client.send(Effect::StartTurn("go".into()));
    common::until(inside).await;
    client.send(Effect::Leave(Departure::Detach));
    client.quit().await;
    daemon.stop().await;
    name
}

/// The next daemon, and a client of the agent that joined its resumed run.
async fn resumed(ws: &Workspace, llm: &Arc<ScriptedLlmClient>, name: String) -> (Running, Client) {
    let daemon = Running::start(&ws.root, llm).await;
    let mut client = Client::attach_agent(&ws.root, AgentChoice::Agent { name }).await;
    client
        .saw(|m| matches!(m, Msg::Agent(env) if matches!(env.payload, AgentEvent::RunResumed { .. })))
        .await;
    client.saw(|m| matches!(m, Msg::RemoteTurn { .. })).await;
    (daemon, client)
}

/// Esc on the joined run, the message typed meanwhile, and a new message:
/// the run stops at once by its id, the message then runs, and the next
/// one gets its model call.
async fn interrupt_and_go_on(daemon: &Running, client: &mut Client, llm: &ScriptedLlmClient) {
    client.send(Effect::Steer {
        id: PendingId(1),
        text: "typed meanwhile".into(),
    });
    client
        .saw(|m| {
            matches!(
                m,
                Msg::SteerAck {
                    id: PendingId(1),
                    result: Ok(())
                }
            )
        })
        .await;
    assert!(!agent_heard(llm, "typed meanwhile"), "the run holds it");
    client.send(Effect::Interrupt);
    assert_eq!(client.turn_finished().await, Err(TurnFailure::Interrupted));
    // The run-fenced stop took the run: no force-cancel that waits for a
    // boundary, no hard cancel after the interrupt's deadline.
    assert_eq!(daemon.forwarded(STOP_MEMBER_RUN), 1);
    assert_eq!(daemon.forwarded(FORCE_CANCEL), 0);
    assert_eq!(daemon.forwarded(HARD_CANCEL_MEMBER), 0);
    assert!(
        !client.notices().contains("did not end within"),
        "{}",
        client.notices()
    );
    // The message typed meanwhile reached the agent, and so does the next.
    common::until(|| agent_heard(llm, "typed meanwhile")).await;
    client.send(Effect::StartTurn("after the stop".into()));
    assert_eq!(
        client.turn_finished().await.unwrap().text,
        "answered after the stop"
    );
    daemon.idle(&daemon.agent).await;
}

/// The daemon's stop catches the agent inside its `delegate` call while
/// the helper works. The next daemon resumes the run and dispatches the
/// call again: the helper starts its task over and the agent shows as
/// running, truly. Esc stops the run at once; before, the client knew no
/// run id for a resumed run, force-cancelled the member (which waits for a
/// boundary the `delegate` holds off) and the run ended only when the hard
/// cancel took it after the interrupt's 10 s deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_stopped_inside_a_delegate_resumes_and_esc_stops_it_at_once() {
    let ws = Workspace::new();
    let llm = script(delegate());
    let helper_calls = || {
        llm.recorded()
            .iter()
            .filter(|r| r.user().contains("HELPER-TASK"))
            .count()
    };
    let name = stop_inside(&ws, &llm, || helper_calls() == 1).await;
    let (daemon, mut client) = resumed(&ws, &llm, name).await;
    // The call was dispatched again: its helper is seated and runs anew.
    common::until(|| helper_calls() == 2).await;
    let connection = daemon.connect().await;
    let mob = format!("implicit-{}", daemon.session);
    common::until_async(async || members(&connection, &mob).await == ["slow"]).await;
    let row = daemon.status().await.agents.remove(0);
    assert_eq!(row.running, Some(true));
    interrupt_and_go_on(&daemon, &mut client, &llm).await;
    client.quit().await;
    daemon.stop().await;
}

/// A helper of a resumed run that the user stops is told to the agent in
/// that run (the resumed run counts as the agent's run, so the agent is not
/// "just interrupted" by the stop any more), and the run goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_the_helper_of_a_resumed_run_tells_the_agent_in_that_run() {
    let ws = Workspace::new();
    let llm = script(delegate());
    let helper_calls = || {
        llm.recorded()
            .iter()
            .filter(|r| r.user().contains("HELPER-TASK"))
            .count()
    };
    let name = stop_inside(&ws, &llm, || helper_calls() == 1).await;
    let (daemon, mut client) = resumed(&ws, &llm, name).await;
    common::until(|| helper_calls() == 2).await;
    client.send(Effect::StopHelper(vec!["slow".into()]));
    let (how, told) = match client
        .saw(|m| matches!(m, Msg::HelperEnded { path, .. } if path.last().is_some_and(|l| l == "slow")))
        .await
    {
        Msg::HelperEnded { how, told, .. } => (how, told),
        _ => unreachable!(),
    };
    assert_eq!(how, "stopped by you");
    assert!(
        told.as_deref().is_some_and(|t| t.ends_with("was told")),
        "{told:?}"
    );
    assert_eq!(client.turn_finished().await.unwrap().text, "went on");
    assert!(
        llm.recorded()
            .iter()
            .any(|r| format!("{:?}", r.messages).contains("The user stopped your helper `slow`."))
    );
    client.quit().await;
    daemon.stop().await;
}

/// The neighbour: the daemon's stop catches the agent inside a slow shell
/// call. The resumed run does not run the command again: the engine gives
/// the call an `outcome_uncertain` result, the run's next model call reads
/// it and the run ends; the client's joined turn ends with it, and the
/// user's next message gets its model call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_stopped_inside_a_shell_call_resumes_with_its_outcome_uncertain() {
    let ws = Workspace::new();
    let llm = script(Step::Tool {
        id: "s1",
        name: "shell",
        args: json!({"command": "echo ran >> runs.txt; sleep 60", "timeout_secs": 60}),
    });
    let runs = ws.root.join("runs.txt");
    let lines = || {
        std::fs::read_to_string(&runs)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    };
    let name = stop_inside(&ws, &llm, || lines() == 1).await;
    let (daemon, mut client) = resumed(&ws, &llm, name).await;
    assert_eq!(client.turn_finished().await.unwrap().text, "went on");
    let read = llm
        .requests()
        .into_iter()
        .rev()
        .find(|messages| results(messages) > 0)
        .unwrap();
    assert!(
        common::tool_text(&read, "s1").contains("outcome_uncertain"),
        "{read:?}"
    );
    assert_eq!(lines(), 1, "the command was not run again");
    client.send(Effect::StartTurn("after the stop".into()));
    assert_eq!(
        client.turn_finished().await.unwrap().text,
        "answered after the stop"
    );
    client.quit().await;
    daemon.stop().await;
}
