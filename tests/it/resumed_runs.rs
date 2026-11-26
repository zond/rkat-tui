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
        .filter(|r| !common::is_helper(&r.messages, "HELPER-TASK"))
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
        if common::is_helper(messages, "HELPER-TASK") {
            return Step::Hang;
        }
        // The user's words among the request's inputs (a restored helper's
        // note can come beside them, before or after).
        let inputs = common::run_inputs(messages);
        if inputs.contains("after the stop") {
            return Step::Text("answered after the stop");
        }
        if inputs.contains("typed meanwhile") {
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

/// The script of the tests of a run with a helper: the agent assigns
/// `HELPER-TASK` to `slow` (whose model calls hang), then its run is held
/// in its next model call until `hold` is notified, which answers "went
/// on"; the later messages are answered as in [`script`].
fn assigning(hold: &Arc<tokio::sync::Notify>) -> Arc<ScriptedLlmClient> {
    let hold = Arc::clone(hold);
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if common::is_helper(messages, "HELPER-TASK") {
            return Step::Hang;
        }
        // The user's words among the request's inputs (a restored helper's
        // note can come beside them, before or after).
        let inputs = common::run_inputs(messages);
        if inputs.contains("after the stop") {
            return Step::Text("answered after the stop");
        }
        if inputs.contains("typed meanwhile") {
            return Step::Text("read the message");
        }
        if results(messages) == 0 {
            return common::assign("a1", "slow", "HELPER-TASK: runs on");
        }
        if common::after(messages, "a1") {
            return Step::Gated(Arc::clone(&hold), Box::new(Step::Text("went on")));
        }
        Step::Text("went on")
    }));
    ScriptedLlmClient::new(vec![step; 20])
}

/// How many model calls the helper (whose task names `HELPER-TASK`) made.
fn helper_calls(llm: &ScriptedLlmClient) -> usize {
    llm.recorded()
        .iter()
        .filter(|r| common::is_helper(&r.messages, "HELPER-TASK"))
        .count()
}

/// How many times the agent made its held model call (the one after its
/// `assign`).
fn held_calls(llm: &ScriptedLlmClient) -> usize {
    llm.recorded()
        .iter()
        .filter(|r| {
            !common::is_helper(&r.messages, "HELPER-TASK") && common::after(&r.messages, "a1")
        })
        .count()
}

/// The agent's run, held in its model call after the `assign` while its
/// helper runs: the daemon's stop catches both there.
fn inside_the_held_call(llm: &ScriptedLlmClient) -> bool {
    held_calls(llm) == 1 && helper_calls(llm) == 1
}

/// Wait until the resumed run made its held model call again and the
/// restored helper called again. A run that went on past that call (the
/// agent's model call read a later result) fails at once, with it.
#[track_caller]
fn redispatched(llm: &ScriptedLlmClient) -> impl Future<Output = ()> + '_ {
    common::until(move || {
        if let Some(results) = went_on(llm) {
            panic!(
                "the resumed run went on past its held call: {results}\nthe daemon's last log lines:\n{}",
                common::log_tail(150)
            );
        }
        held_calls(llm) == 2 && helper_calls(llm) == 2
    })
}

/// The tool results of the agent's first model call that read more than
/// its `assign`'s (its run went on past the call it was stopped in), if it
/// made one.
fn went_on(llm: &ScriptedLlmClient) -> Option<String> {
    llm.recorded()
        .iter()
        .find(|r| !common::is_helper(&r.messages, "HELPER-TASK") && results(&r.messages) > 1)
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
/// the run stops at once by its id, the message comes back unread (the
/// owner's rule), and the next one gets its
/// model call.
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
    // The run-fenced stop took the run by its id, once: no force that
    // waits for a boundary, no hard cancel after the interrupt's deadline,
    // and no engine interrupt method (the client sends `rkat/interrupt`).
    let interrupts = daemon.local().interrupts();
    assert_eq!(interrupts.len(), 1, "{interrupts:#?}");
    assert!(interrupts[0].run_id.is_some());
    assert_eq!(
        interrupts[0].run,
        rkat_tui::daemon::protocol::RunInterrupt::Stopped
    );
    for method in [STOP_MEMBER_RUN, FORCE_CANCEL, HARD_CANCEL_MEMBER] {
        assert_eq!(daemon.forwarded(method), 0, "{method}");
    }
    assert!(
        !client.notices().contains("did not end within"),
        "{}",
        client.notices()
    );
    // The message typed meanwhile came back unread (the owner's
    // rule); the next message gets its model call.
    assert!(
        client
            .seen
            .iter()
            .any(|m| matches!(m, Msg::Returned(ids) if ids == &[PendingId(1)])),
        "{:#?}",
        client.seen
    );
    assert!(!agent_heard(llm, "typed meanwhile"));
    client.send(Effect::StartTurn("after the stop".into()));
    assert_eq!(
        client.turn_finished().await.unwrap().text,
        "answered after the stop"
    );
    daemon.idle(&daemon.agent).await;
}

/// The daemon's stop catches the agent inside a model call while its
/// `assign` helper works. The next daemon resumes the run, which makes
/// that call again, and restores the helper, which runs anew: the agent
/// shows as running, truly. Esc stops the run at once, by its id, not
/// through a force-cancel of the member that waits for a boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_stopped_with_a_helper_resumes_and_esc_stops_it_at_once() {
    let ws = Workspace::new();
    let hold = Arc::new(tokio::sync::Notify::new());
    let llm = assigning(&hold);
    let name = stop_inside(&ws, &llm, || inside_the_held_call(&llm)).await;
    let (daemon, mut client) = resumed(&ws, &llm, name).await;
    // The run made its call again, and its helper runs anew.
    redispatched(&llm).await;
    let connection = daemon.connect().await;
    common::until_async(async || {
        members(&connection, "tui")
            .await
            .iter()
            .any(|m| m == "slow")
    })
    .await;
    let row = daemon.status().await.agents.remove(0);
    assert_eq!(row.running, Some(true));
    interrupt_and_go_on(&daemon, &mut client, &llm).await;
    client.quit().await;
    daemon.stop().await;
}

/// The order the sibling above may meet only on a slow host, every time:
/// the client attaches after the next daemon resumed the run, which made
/// its call again, and the restored helper made its call, so it sees
/// neither the run's `run_resumed` nor any other event of it. The daemon
/// tells it the open run as it attaches: it joins the turn, the message
/// typed meanwhile waits, and Esc stops the run by its id at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_attaching_inside_a_resumed_run_stops_the_run_by_its_id() {
    let ws = Workspace::new();
    let hold = Arc::new(tokio::sync::Notify::new());
    let llm = assigning(&hold);
    let name = stop_inside(&ws, &llm, || inside_the_held_call(&llm)).await;
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
/// Esc stops the run by its id at once, the message typed meanwhile comes
/// back unread, and the next one gets its model call.
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
    let hold = Arc::new(tokio::sync::Notify::new());
    let llm = assigning(&hold);
    let name = stop_inside(&ws, &llm, || inside_the_held_call(&llm)).await;
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
    hold.notify_one();
    assert_eq!(client.turn_finished().await.unwrap().text, "went on");
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| format!("{:?}", r.messages).contains("The user stopped your helper `slow`."))
    })
    .await;
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

/// The start reads each agent's tool catalog, and the engine's read waits
/// behind a running turn, up to 30 s; on a slow host the restore has
/// already resumed the agent's run by then. Here the start is held until
/// the resumed run made its call again, so the catalog read meets a
/// running turn every time; the daemon must still serve at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_start_does_not_wait_behind_a_resumed_run_s_turn() {
    let ws = Workspace::new();
    let hold = Arc::new(tokio::sync::Notify::new());
    let llm = assigning(&hold);
    let name = stop_inside(&ws, &llm, || inside_the_held_call(&llm)).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    let setup = rig::Setup::default();
    let args = setup.command_line(&ws.root);
    let options = setup
        .options(&ws.root, &llm)
        .with_before_catalogs(Arc::clone(&gate));
    let start = tokio::spawn(async move { Running::try_start_with(&args, options).await });
    // The resumed run is in its held call, and its helper called again.
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
    hold.notify_one();
    assert_eq!(client.turn_finished().await.unwrap().text, "went on");
    client.quit().await;
    daemon.stop().await;
}
