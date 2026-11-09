//! Helpers in attach mode (P4): the daemon's orchestrator delegates with
//! the engine's `delegate`, and the attached TUI shows the helper as the
//! in-process follower does (`[helper · model]`, its events nested, its
//! result, the running count, its usage in `/cost`), from the mobs' own
//! event streams. Esc stops the orchestrator's run and its helpers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use std::sync::Arc;
use std::time::Instant;

use common::{MODEL, ScriptedLlmClient, Step, Workspace, within};
use meerkat_core::{AgentEvent, StreamScopeFrame};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rig::{Client, Running};
use rkat_tui::ui::{App, Effect, Msg, StatusInfo, StatusUpdate, TurnFailure};
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

fn no_helpers(msg: &Msg) -> bool {
    matches!(msg, Msg::Status(StatusUpdate::Helpers(h)) if h.is_empty())
}

fn one_helper(label: &'static str) -> impl Fn(&Msg) -> bool {
    move |msg| matches!(msg, Msg::Status(StatusUpdate::Helpers(h)) if h.len() == 1 && h[0].label == label)
}

/// The helper label and depth of a scoped message.
fn helper_of(msg: &Msg) -> Option<(String, usize)> {
    let Msg::Scoped(ev) = msg else { return None };
    let frames: Vec<&str> = ev
        .scope_path
        .iter()
        .filter_map(|f| match f {
            StreamScopeFrame::MobMember { agent_identity, .. } => Some(agent_identity.as_str()),
            _ => None,
        })
        .collect();
    frames
        .last()
        .map(|label| ((*label).to_string(), frames.len()))
}

/// The rows of `app` drawn into an 80×24 terminal.
fn draw(app: &mut App) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| rkat_tui::ui::render::draw_at(f, app, Instant::now()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegated_helper_is_followed_with_its_result_and_cost() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        delegate("d1", "scout", "HELPER-TASK: look around"),
        Step::Gated(Arc::clone(&gate), Box::new(Step::Text("HELPER-DONE"))),
        Step::Text("parent done"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("USER-ASK delegate it".into()));
    // The helper runs (its model call waits on the gate): the TUI lists it.
    llm.wait_call(1).await;
    client.saw(one_helper("scout")).await;
    gate.notify_one();
    let outcome = client.turn_finished().await;
    assert_eq!(outcome.result.unwrap().text, "parent done");
    client.saw(no_helpers).await;
    client
        .saw(|m| matches!(m, Msg::Scoped(ev) if matches!(&ev.event, AgentEvent::TextComplete { content, .. } if content == "HELPER-DONE")))
        .await;

    let recorded = llm.recorded();
    assert!(recorded[0].tools.iter().any(|t| t == "delegate"));
    assert!(recorded[1].user().contains("HELPER-TASK"));
    assert_eq!(recorded[1].model, MODEL);
    // Its events reached the UI once each, nested one level and named.
    let scoped: Vec<_> = client.seen.iter().filter_map(helper_of).collect();
    assert!(!scoped.is_empty());
    assert!(
        scoped
            .iter()
            .all(|(label, depth)| label == "scout" && *depth == 1)
    );
    let starts = client
        .seen
        .iter()
        .filter(
            |m| matches!(m, Msg::Scoped(ev) if matches!(ev.event, AgentEvent::RunStarted { .. })),
        )
        .count();
    assert_eq!(starts, 1, "the backfill showed the helper's start once");
    assert!(client.seen.iter().any(|m| matches!(
        m,
        Msg::Status(StatusUpdate::Helpers(h)) if h.len() == 1 && h[0].model == MODEL && h[0].depth == 1
    )));

    // The UI over what the runner said: the follower line, the result, the
    // cost summed across the orchestrator and the helper.
    let mut app = App::new(StatusInfo::default());
    for msg in client.seen.clone() {
        app.update(msg);
    }
    let rows = draw(&mut app);
    assert!(
        rows.iter()
            .any(|row| row.contains(&format!("[scout · {MODEL}]"))),
        "{rows:#?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("parent done")),
        "{rows:#?}"
    );
    let helper = &app.status.cost().helpers["scout"][MODEL];
    assert_eq!(helper.input, 10, "one helper call");
    assert_eq!(
        app.status.cost().per_model[MODEL].input,
        30,
        "two calls of the orchestrator and one of the helper"
    );
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn esc_during_a_helper_stops_the_orchestrator_and_the_helper() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        delegate("d1", "slow", "wait forever"),
        Step::Hang,
        Step::Text("after"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    llm.wait_call(1).await;
    client.saw(one_helper("slow")).await;
    client.send(Effect::Interrupt);
    let outcome = client.turn_finished().await;
    assert_eq!(outcome.result, Err(TurnFailure::Interrupted), "{outcome:?}");
    client.saw(no_helpers).await;
    // The daemon retired the helper with its parent's run.
    let connection = daemon.connect().await;
    let mob = format!("implicit-{}", daemon.session);
    within(async {
        loop {
            let members = connection
                .call("mob/members", json!({"mob_id": mob}))
                .await
                .unwrap();
            if members["members"].as_array().is_some_and(Vec::is_empty) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    // The orchestrator serves the next prompt.
    client.send(Effect::StartTurn("again".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "after");
    client.quit().await;
    daemon.stop().await;
}
