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
    script_gated(call, None)
}

/// [`script`], its `went on` held until `gate` is notified.
fn script_gated(call: Step, gate: Option<Arc<tokio::sync::Notify>>) -> Arc<ScriptedLlmClient> {
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
        match (results(messages), &gate) {
            (0, _) => call.clone(),
            (_, None) => Step::Text("went on"),
            (_, Some(gate)) => Step::Gated(Arc::clone(gate), Box::new(Step::Text("went on"))),
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

/// How many model calls the helper (whose task names `HELPER-TASK`) made.
fn helper_calls(llm: &ScriptedLlmClient) -> usize {
    llm.recorded()
        .iter()
        .filter(|r| r.user().contains("HELPER-TASK"))
        .count()
}

/// Wait until the resumed run's `delegate`, dispatched again, seated its
/// new helper and that helper called (the second helper call). A run that
/// went on without it (the agent's next model call read the call's result)
/// fails at once, with that result.
#[track_caller]
fn redispatched(llm: &ScriptedLlmClient) -> impl Future<Output = ()> + '_ {
    common::until(move || {
        if let Some(results) = went_on(llm) {
            panic!(
                "the resumed run went on without seating the helper again: {results}\nthe daemon's last log lines:\n{}",
                common::log_tail(150)
            );
        }
        helper_calls(llm) == 2
    })
}

/// The tool results of the agent's first model call that read one (its run
/// went on past the call it was stopped in), if it made one.
fn went_on(llm: &ScriptedLlmClient) -> Option<String> {
    llm.recorded()
        .iter()
        .find(|r| !r.user().contains("HELPER-TASK") && results(&r.messages) > 0)
        .map(|r| {
            r.messages
                .iter()
                .filter(|m| matches!(m, Message::ToolResults { .. }))
                .map(|m| format!("{m:?}"))
                .collect::<Vec<_>>()
                .join("; ")
        })
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
/// The client attaches once the engine runs the resumed run, so it sees no
/// `run_resumed`: the daemon tells it the open run's id as it attaches.
async fn resumed(ws: &Workspace, llm: &Arc<ScriptedLlmClient>, name: String) -> (Running, Client) {
    let daemon = Running::start(&ws.root, llm).await;
    let client = join(&daemon, &ws.root, name, Some(llm)).await;
    (daemon, client)
}

/// Once the agent's run is open, a client of the agent `name`, which
/// joined its turn. With `resumed`, a run that went on past the call it
/// was stopped in fails the wait at once, with what it read.
async fn join(
    daemon: &Running,
    root: &std::path::Path,
    name: String,
    resumed: Option<&ScriptedLlmClient>,
) -> Client {
    common::until_async(async || {
        if let Some(results) = resumed.and_then(went_on) {
            panic!(
                "the resumed run went on before the client joined it: {results}\nthe daemon's last log lines:\n{}",
                common::log_tail(150)
            );
        }
        daemon.status().await.agents[0].running == Some(true)
    })
    .await;
    let mut client = Client::attach_agent(root, AgentChoice::Agent { name }).await;
    client.saw(|m| matches!(m, Msg::RemoteTurn { .. })).await;
    client
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
    redispatched(&llm).await;
    let connection = daemon.connect().await;
    let mob = format!("implicit-{}", daemon.session);
    common::until_async(async || members(&connection, &mob).await == ["slow"]).await;
    let row = daemon.status().await.agents.remove(0);
    assert_eq!(row.running, Some(true));
    interrupt_and_go_on(&daemon, &mut client, &llm).await;
    client.quit().await;
    daemon.stop().await;
}

/// The order the sibling above may meet only on a slow host, every time:
/// the client attaches after the next daemon resumed the run and the
/// helper of its `delegate` made its call, so it sees neither the run's
/// `run_resumed` nor any other event of it. The daemon tells it the open
/// run as it attaches: it joins the turn, the message typed meanwhile
/// waits, and Esc stops the run by its id at once. Before, it showed no
/// turn at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_attaching_inside_a_resumed_delegate_stops_the_run_by_its_id() {
    let ws = Workspace::new();
    let llm = script(delegate());
    let helper_calls = || {
        llm.recorded()
            .iter()
            .filter(|r| r.user().contains("HELPER-TASK"))
            .count()
    };
    let name = stop_inside(&ws, &llm, || helper_calls() == 1).await;
    let daemon = Running::start(&ws.root, &llm).await;
    redispatched(&llm).await;
    let mut client = join(&daemon, &ws.root, name, Some(&llm)).await;
    assert!(
        !client
            .events()
            .iter()
            .any(|e| matches!(e, AgentEvent::RunResumed { .. })),
        "the run's resumption fell before the attach"
    );
    interrupt_and_go_on(&daemon, &mut client, &llm).await;
    client.quit().await;
    daemon.stop().await;
}

/// No restart: a client detaches while the agent's run is inside a slow
/// shell call, and a client that attaches again joins that turn from the
/// daemon's note (no event of the run comes while the command sleeps).
/// Esc stops the run by its id at once, the message typed meanwhile then
/// gets its model call, and so does the next. Before, the attaching client
/// showed no turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_reattaching_inside_a_slow_shell_call_stops_the_run_by_its_id() {
    let ws = Workspace::new();
    let llm = script(Step::Tool {
        id: "s1",
        name: "shell",
        args: json!({"command": "echo ran >> runs.txt; sleep 60", "timeout_secs": 60}),
    });
    let runs = ws.root.join("runs.txt");
    let daemon = Running::start(&ws.root, &llm).await;
    let mut first = Client::attach(&ws.root).await;
    let name = first.agent().await;
    first.send(Effect::StartTurn("go".into()));
    common::until(|| runs.exists()).await;
    first.send(Effect::Leave(Departure::Detach));
    first.quit().await;
    let mut client = join(&daemon, &ws.root, name, None).await;
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
    redispatched(&llm).await;
    client.send(Effect::StopHelper(vec!["slow".into()]));
    // The stop's ending; the helper the last daemon left under the same
    // name may be told ended first (`LEFT_ENDED`).
    let (how, told) = match client
        .saw(|m| matches!(m, Msg::HelperEnded { path, how, .. } if path.last().is_some_and(|l| l == "slow") && how != LEFT_ENDED))
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
    // The resumed run's model call waits for the client to join it.
    let joined = Arc::new(tokio::sync::Notify::new());
    let llm = script_gated(
        Step::Tool {
            id: "s1",
            name: "shell",
            args: json!({"command": "echo ran >> runs.txt; sleep 60", "timeout_secs": 60}),
        },
        Some(Arc::clone(&joined)),
    );
    let runs = ws.root.join("runs.txt");
    let lines = || {
        std::fs::read_to_string(&runs)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    };
    let name = stop_inside(&ws, &llm, || lines() == 1).await;
    // Its resumed run goes on past the call by design: no check of that.
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = join(&daemon, &ws.root, name, None).await;
    joined.notify_one();
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

/// How the start tells a helper the last daemon left, its mob no longer
/// seating it.
const LEFT_ENDED: &str = "ended when the daemon stopped";

/// The cause of `stopping_the_helper_of_a_resumed_run_tells_the_agent_in_that_run`
/// failing on a slow runner (`within` of the second daemon's start ran out):
/// the start read each agent's tool catalog, and the engine's read waits
/// behind a running turn, up to 30 s; on a slow host the restore had
/// already resumed the agent's run, inside its `delegate` call waiting for
/// its hanging helper, so the start (and every client) waited the 30 s.
/// Here the start is held until the resumed run's helper made its call, so
/// the catalog read meets a running turn every time; the daemon must still
/// serve at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_start_does_not_wait_behind_a_resumed_run_s_turn() {
    let ws = Workspace::new();
    let llm = script(delegate());
    let helper_calls = {
        let llm = Arc::clone(&llm);
        move || {
            llm.recorded()
                .iter()
                .filter(|r| r.user().contains("HELPER-TASK"))
                .count()
        }
    };
    let name = stop_inside(&ws, &llm, || helper_calls() == 1).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    let setup = rig::Setup::default();
    let args = setup.command_line(&ws.root);
    let options = setup
        .options(&ws.root, &llm)
        .with_before_catalogs(Arc::clone(&gate));
    let start = tokio::spawn(async move { Running::try_start_with(&args, options).await });
    // The resumed run is in its `delegate` call: its new helper called.
    redispatched(&llm).await;
    gate.notify_one();
    let started = std::time::Instant::now();
    let daemon = start.await.unwrap().expect("the daemon starts");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "the start waited {:?} after the hold",
        started.elapsed()
    );
    let mut client = Client::attach_agent(&ws.root, AgentChoice::Agent { name }).await;
    client.send(Effect::StopHelper(vec!["slow".into()]));
    let how = match client
        .saw(|m| matches!(m, Msg::HelperEnded { path, how, .. } if path.last().is_some_and(|l| l == "slow") && how != LEFT_ENDED))
        .await
    {
        Msg::HelperEnded { how, .. } => how,
        _ => unreachable!(),
    };
    assert_eq!(how, "stopped by you");
    assert_eq!(client.turn_finished().await.unwrap().text, "went on");
    client.quit().await;
    daemon.stop().await;
}
