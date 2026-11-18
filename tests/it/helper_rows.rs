//! The client's live helper rows (`/helpers`' live view and the panel
//! under the input) against what the daemon's mob seats: after every kind
//! of ending, a re-attach, a `/resume` and a daemon restart, the helpers
//! the client shows live are exactly the helpers the engine seats for the
//! agent's tree.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, is_helper, start};
use meerkat_core::Message;
use rig::{Client, Running, Setup};
use rkat_tui::client::transport::Connection;
use rkat_tui::ui::{App, Effect, StatusInfo};
use serde_json::json;

/// The text of the last user message of `messages`.
fn last_user(messages: &[Message]) -> Option<String> {
    match messages.last() {
        Some(Message::User(user)) => Some(user.text_content()),
        _ => None,
    }
}

fn retire(id: &'static str, name: &str) -> Step {
    Step::Tool {
        id,
        name: "retire_helper",
        args: json!({"name": name}),
    }
}

/// The script: the agent starts helpers of every kind on `PHASE-START`,
/// retires `k1` (idle) on `PHASE-RETIRE-IDLE`, `r1` (running) on
/// `PHASE-RETIRE-RUNNING`, starts `p1` (which keeps `c1` of its own) on
/// `PHASE-NEST` and retires `p1` on `PHASE-RETIRE-PARENT`; it answers
/// every other input with "noted". `k1`, `k2` and `s1` answer at once,
/// `f1` fails, `r1` and `r2` never end their first call.
fn script() -> Arc<ScriptedLlmClient> {
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        for marker in ["K1-TASK", "K2-TASK", "S1-TASK", "C1-TASK"] {
            if is_helper(messages, marker) {
                return Step::Text("HELPER-DONE");
            }
        }
        if is_helper(messages, "F1-TASK") {
            return Step::RejectTools;
        }
        if is_helper(messages, "R1-TASK") || is_helper(messages, "R2-TASK") {
            return Step::Hang;
        }
        if is_helper(messages, "P1-TASK") {
            if messages
                .iter()
                .any(|m| matches!(m, Message::ToolResults { .. }))
            {
                return Step::Text("P1-WAITING");
            }
            return start("p-c1", "c1", "C1-TASK", "fresh", true);
        }
        let Some(user) = last_user(messages) else {
            return Step::Text("noted");
        };
        if user.contains("PHASE-START") {
            return Step::Tools(vec![
                (
                    "t-k1",
                    "assign",
                    json!({"name": "k1", "task": "K1-TASK", "keep": true}),
                ),
                (
                    "t-k2",
                    "assign",
                    json!({"name": "k2", "task": "K2-TASK", "keep": true}),
                ),
                ("t-s1", "assign", json!({"name": "s1", "task": "S1-TASK"})),
                ("t-f1", "assign", json!({"name": "f1", "task": "F1-TASK"})),
                (
                    "t-r1",
                    "assign",
                    json!({"name": "r1", "task": "R1-TASK", "keep": true}),
                ),
                (
                    "t-r2",
                    "assign",
                    json!({"name": "r2", "task": "R2-TASK", "keep": true}),
                ),
            ]);
        }
        if user.contains("PHASE-RETIRE-IDLE") {
            return retire("t-rk1", "k1");
        }
        if user.contains("PHASE-RETIRE-RUNNING") {
            return retire("t-rr1", "r1");
        }
        if user.contains("PHASE-NEST") {
            return start("t-p1", "p1", "P1-TASK", "fresh", true);
        }
        if user.contains("PHASE-RETIRE-PARENT") {
            return retire("t-rp1", "p1");
        }
        Step::Text("noted")
    }));
    ScriptedLlmClient::new(vec![step; 200])
}

/// The helpers the workspace's mob seats (its agents left out).
async fn seated(connection: &Connection) -> BTreeSet<String> {
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

/// The UI over everything `client` said so far.
fn app_of(client: &mut Client) -> App {
    client.drain();
    let mut app = App::new(StatusInfo::default());
    for msg in client.seen.clone() {
        app.update(msg);
    }
    app
}

/// The helpers the client shows live: `/helpers`' live rows.
fn live_rows(app: &App) -> BTreeSet<String> {
    app.transcript
        .helper_headers()
        .filter(|(_, fold)| app.transcript.helper_live(fold))
        .map(|(_, fold)| fold.label.clone())
        .collect()
}

/// Wait until the mob seats exactly `expected` and the client's live rows
/// are the same, and the panel lists no helper that is not live.
#[track_caller]
fn rows_are<'a>(
    client: &'a mut Client,
    connection: &'a Connection,
    expected: &'a [&'a str],
) -> impl Future<Output = ()> + 'a {
    let at = std::panic::Location::caller();
    async move {
        let expected: BTreeSet<String> = expected.iter().map(ToString::to_string).collect();
        let deadline = tokio::time::Instant::now() + common::TIMEOUT;
        loop {
            let mob = seated(connection).await;
            let app = app_of(client);
            let rows = live_rows(&app);
            let panel: BTreeSet<String> = app
                .helper_panel()
                .into_iter()
                .filter_map(|id| match app.transcript.block(id) {
                    Some(rkat_tui::ui::transcript::Block::Helper(fold))
                        if app.transcript.helper_live(fold) =>
                    {
                        Some(fold.label.clone())
                    }
                    _ => None,
                })
                .collect();
            if mob == expected && rows == expected && panel == expected {
                return;
            }
            if tokio::time::Instant::now() > deadline {
                common::timed_out(
                    at,
                    &format!(
                        "; expected {expected:?}, the mob seats {mob:?}, the client's live rows {rows:?}, its panel's live rows {panel:?}"
                    ),
                );
            }
            tokio::time::sleep(common::POLL).await;
        }
    }
}

/// Every kind of ending leaves the client's live rows equal to the mob's
/// seated helpers: single-use finished, failed, a kept helper retired by
/// its agent while idle and while running, one stopped by the user, a
/// parent retired with its own helper; then a re-attach, a `/resume` of
/// the conversation and a daemon restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_client_s_live_helper_rows_are_the_mob_s_seated_helpers_after_every_ending() {
    use rkat_tui::daemon::protocol::AgentChoice;
    use rkat_tui::ui::Departure;
    let ws = Workspace::new();
    let llm = script();
    let setup = Setup::args(&["--set", "helpers.max_depth=2"]);
    let args = setup.command_line(&ws.root);
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let connection = daemon.connect().await;
    let mut client = Client::attach_args(&ws.root, &args, None).await;
    let name = client.agent().await;

    // s1 finishes and f1 fails (both retired); k1, k2 stay idle; r1, r2 run.
    client.send(Effect::StartTurn("PHASE-START".into()));
    rows_are(&mut client, &connection, &["k1", "k2", "r1", "r2"]).await;
    daemon.idle(&daemon.agent).await;

    // The agent retires an idle kept helper: no run of it ends.
    client.send(Effect::StartTurn("PHASE-RETIRE-IDLE".into()));
    rows_are(&mut client, &connection, &["k2", "r1", "r2"]).await;
    daemon.idle(&daemon.agent).await;

    // The agent retires a running kept helper.
    client.send(Effect::StartTurn("PHASE-RETIRE-RUNNING".into()));
    rows_are(&mut client, &connection, &["k2", "r2"]).await;
    daemon.idle(&daemon.agent).await;

    // The user stops a running helper.
    client.send(Effect::StopHelper(vec!["r2".into()]));
    rows_are(&mut client, &connection, &["k2"]).await;
    daemon.idle(&daemon.agent).await;

    // A helper with a helper of its own, then the parent retired.
    client.send(Effect::StartTurn("PHASE-NEST".into()));
    rows_are(&mut client, &connection, &["c1", "k2", "p1"]).await;
    daemon.idle(&daemon.agent).await;
    client.send(Effect::StartTurn("PHASE-RETIRE-PARENT".into()));
    // Its own helper c1 is retired with it, and the answer says so.
    rows_are(&mut client, &connection, &["k2"]).await;
    daemon.idle(&daemon.agent).await;
    assert!(
        llm.recorded().iter().any(|r| format!("{:?}", r.messages)
            .contains("Helper `p1` was retired. Its own helper `c1` was retired with it.")),
        "the retire's answer names c1"
    );

    // A client that attaches now sees the same.
    client.send(Effect::Leave(Departure::Detach));
    client.quit().await;
    let mut client =
        Client::attach_agent(&ws.root, AgentChoice::Agent { name: name.clone() }).await;
    rows_are(&mut client, &connection, &["k2"]).await;

    // A restart: the kept helpers are restored, and listed live.
    client.send(Effect::Leave(Departure::Detach));
    client.quit().await;
    drop(connection);
    daemon.stop().await;
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let connection = daemon.connect().await;
    let mut client = Client::attach_agent(&ws.root, AgentChoice::Agent { name }).await;
    rows_are(&mut client, &connection, &["k2"]).await;

    // A new conversation ends the helpers of the one it leaves; resumed,
    // that one lists none live.
    let session = daemon.current_session().await;
    client.send(Effect::NewSession);
    client
        .saw(|m| matches!(m, rkat_tui::ui::Msg::Notice { text, .. } if text.starts_with("a new conversation")))
        .await;
    rows_are(&mut client, &connection, &[]).await;
    client.send(Effect::ResumeSession(session.to_string()));
    client
        .saw(|m| matches!(m, rkat_tui::ui::Msg::Notice { text, .. } if text.starts_with("resumed conversation")))
        .await;
    rows_are(&mut client, &connection, &[]).await;
    client.quit().await;
    drop(connection);
    daemon.stop().await;
}

/// Whether one of the agent's requests holds `text`.
fn agent_heard(llm: &ScriptedLlmClient, text: &str) -> bool {
    llm.recorded()
        .iter()
        .filter(|r| !is_helper(&r.messages, "-TASK"))
        .any(|r| format!("{:?}", r.messages).contains(text))
}

/// Nothing stays seated without a living owner: a single-use helper that
/// finishes takes its own kept helper with it, and so does a running kept
/// helper the user stops (its running helper is stopped, then retired);
/// each parent's note names the helper retired with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_s_own_helpers_are_retired_with_it_however_it_ends() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let results = messages
            .iter()
            .any(|m| matches!(m, Message::ToolResults { .. }));
        if is_helper(messages, "C2-TASK") {
            return Step::Text("C2-DONE");
        }
        if is_helper(messages, "C3-TASK") {
            return Step::Hang;
        }
        if is_helper(messages, "S2-TASK") {
            if results {
                return Step::Text("S2-DONE");
            }
            return start("t-c2", "c2", "C2-TASK", "fresh", true);
        }
        if is_helper(messages, "U1-TASK") {
            if results {
                return Step::Hang;
            }
            return start("t-c3", "c3", "C3-TASK", "fresh", true);
        }
        match last_user(messages) {
            Some(user) if user.contains("PHASE-SINGLE") => {
                start("t-s2", "s2", "S2-TASK", "fresh", false)
            }
            Some(user) if user.contains("PHASE-STOP") => {
                start("t-u1", "u1", "U1-TASK", "fresh", true)
            }
            _ => Step::Text("noted"),
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 60]);
    let setup = Setup::args(&["--set", "helpers.max_depth=2"]);
    let args = setup.command_line(&ws.root);
    let daemon = Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let connection = daemon.connect().await;
    let mut client = Client::attach_args(&ws.root, &args, None).await;

    // s2 starts c2 (kept, it goes idle) and finishes: both go.
    client.send(Effect::StartTurn("PHASE-SINGLE".into()));
    common::until(|| {
        agent_heard(&llm, "Your helper `s2` finished its task")
            && agent_heard(&llm, "Its own helper `c2` was retired with it.")
    })
    .await;
    rows_are(&mut client, &connection, &[]).await;
    daemon.idle(&daemon.agent).await;

    // u1 starts c3 (running) and runs on; the user stops u1: both go.
    client.send(Effect::StartTurn("PHASE-STOP".into()));
    rows_are(&mut client, &connection, &["c3", "u1"]).await;
    daemon.idle(&daemon.agent).await;
    client.send(Effect::StopHelper(vec!["u1".into()]));
    rows_are(&mut client, &connection, &[]).await;
    common::until(|| {
        agent_heard(
            &llm,
            "The user stopped your helper `u1`. Its file changes stay on disk; its conversation is not continued. Its own helper `c3` was retired with it.",
        )
    })
    .await;
    daemon.idle(&daemon.agent).await;
    client.quit().await;
    drop(connection);
    daemon.stop().await;
}
