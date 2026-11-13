//! Helpers: the daemon's agent delegates with
//! the engine's `delegate`, and the attached TUI shows the helper
//! (`[helper · model]`, its events nested, its
//! result, the running count, its usage in `/cost`), from the mobs' own
//! event streams. Esc stops the agent's run and its helpers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;
use std::time::Instant;

use common::{MODEL, ScriptedLlmClient, Step, Workspace, within};
use meerkat_core::{AgentEvent, Message, StreamScopeFrame};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rig::{Client, Running};
use rkat_tui::client::transport::Connection;
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
    assert_eq!(outcome.unwrap().text, "parent done");
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
    // cost summed across the agent and the helper.
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
        "two calls of the agent and one of the helper"
    );
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn esc_during_a_helper_stops_the_agent_and_the_helper() {
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
    assert_eq!(outcome, Err(TurnFailure::Interrupted), "{outcome:?}");
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
    // The agent serves the next prompt.
    client.send(Effect::StartTurn("again".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "after");
    client.quit().await;
    daemon.stop().await;
}

/// The adoption test of the lukacf/meerkat#2102 fix on the daemon: after a
/// `/model` switch the agent's `delegate` seats its helper on the
/// switched model (the helper inherits the session's identity as it is
/// now, not the one the agent was spawned with).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_delegated_after_a_model_switch_runs_on_the_new_model() {
    const SWITCHED: &str = "claude-opus-4-8";
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Text("one"),
        delegate("d1", "scout", "say hi"),
        Step::Text("hi"),
        Step::Text("done"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("first".into()));
    client.turn_finished().await.unwrap();
    client.send(Effect::SetModel(format!("anthropic:{SWITCHED}")));
    client
        .wait_for(
            |m| matches!(m, Msg::Status(StatusUpdate::Model { model, .. }) if model == SWITCHED),
        )
        .await;
    client.send(Effect::StartTurn("delegate".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "done");
    let recorded = llm.recorded();
    assert_eq!(recorded[0].model, MODEL);
    assert_eq!(recorded[1].model, SWITCHED);
    assert_eq!(
        recorded[2].model, SWITCHED,
        "the helper runs on the new model"
    );
    // The helper's own session records it.
    let result = recorded[3]
        .messages
        .iter()
        .find_map(|m| match m {
            meerkat_core::Message::ToolResults { results, .. } => {
                results.iter().find(|r| r.tool_use_id == "d1").cloned()
            }
            _ => None,
        })
        .expect("the delegate result");
    let value: serde_json::Value = serde_json::from_str(&result.text_content())
        .unwrap_or_else(|_| panic!("delegate failed: {}", result.text_content()));
    let helper = meerkat_core::SessionId::parse(value["session_id"].as_str().unwrap()).unwrap();
    client.quit().await;
    daemon.stop().await;
    let meta = rig::stored_session(&ws.root, &helper)
        .await
        .session_metadata()
        .unwrap();
    assert_eq!(meta.model, SWITCHED);
}

/// The member identities `mob` lists, by `connection`.
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

/// Wait until `mob` lists no member.
async fn until_empty(connection: &Connection, mob: &str) {
    within(async {
        while !members(connection, mob).await.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
}

/// The realm config of `ws` with `delegate`'s tool timeout at 3 s.
fn delegate_times_out(ws: &Workspace) {
    let config = common::realm_of(&ws.root).join("config.toml");
    std::fs::write(&config, "[tools.tool_timeouts]\ndelegate = \"3s\"\n").unwrap();
}

/// A `delegate` call that times out (the engine's tool timeout, here 3 s
/// from the realm's `[tools.tool_timeouts] delegate`) leaves no helper
/// seated: the engine drops the call without retiring its helper, and the
/// daemon retires it when the agent's stream says the call ended with an
/// error (`rkat_tui::mobs::delegates`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_timed_out_delegate_leaves_no_helper_seated() {
    let ws = Workspace::new();
    delegate_times_out(&ws);
    let llm = ScriptedLlmClient::new(vec![
        delegate("d1", "stuck", "never finishes"),
        Step::Hang,
        Step::Text("after the timeout"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    llm.wait_call(1).await;
    client.saw(one_helper("stuck")).await;
    // The call times out; the agent goes on with the error.
    let outcome = client.turn_finished().await;
    assert_eq!(outcome.unwrap().text, "after the timeout");
    assert!(client.events().iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionTimedOut { name, .. } if name == "delegate"
    )));
    let connection = daemon.connect().await;
    until_empty(&connection, &format!("implicit-{}", daemon.session)).await;
    client.saw(no_helpers).await;
    client.quit().await;
    daemon.stop().await;
}

/// A helper whose own turn fails ends its `delegate` call with an error,
/// and the engine retires it then; nothing stays seated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegate_whose_helper_fails_leaves_no_helper_seated() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if common::user_texts(messages)
            .concat()
            .contains("fail please")
        {
            // The helper's model call: a 400, which is not retried.
            return Step::RejectTools;
        }
        match messages
            .iter()
            .filter(|m| matches!(m, Message::ToolResults { .. }))
            .count()
        {
            0 => delegate("d1", "doomed", "fail please"),
            _ => Step::Text("after the failure"),
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 10]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    let outcome = client.turn_finished().await;
    assert_eq!(outcome.unwrap().text, "after the failure");
    assert!(client.events().iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionCompleted { name, is_error: true, .. } if name == "delegate"
    )));
    let connection = daemon.connect().await;
    until_empty(&connection, &format!("implicit-{}", daemon.session)).await;
    client.quit().await;
    daemon.stop().await;
}

/// The retire is the timed-out call's helper's only: a `spawn_member`
/// member that is meant to stay (here with the same member id, in its own
/// mob) is still seated after a `delegate` of the same run timed out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_timed_out_delegate_leaves_a_long_lived_member_seated() {
    let ws = Workspace::new();
    delegate_times_out(&ws);
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let user = common::user_texts(messages).concat();
        if user.contains("hello") {
            // The team member's own turn.
            return Step::Text("ready");
        }
        if user.contains("never finishes") {
            // The helper's model call.
            return Step::Hang;
        }
        match messages
            .iter()
            .filter(|m| matches!(m, Message::ToolResults { .. }))
            .count()
        {
            0 => Step::Tool {
                id: "m1",
                name: "mob_create",
                args: json!({"definition": {"id": "team", "profiles": {"worker": {"model": MODEL, "tools": {"comms": true}}}}}),
            },
            1 => Step::Tool {
                id: "s1",
                name: "mob_spawn_member",
                args: json!({"mob_id": "team", "profile": "worker", "member_id": "w1", "initial_message": "hello"}),
            },
            2 => delegate("d1", "w1", "never finishes"),
            _ => Step::Text("after the timeout"),
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 20]);
    let daemon = Running::start(&ws.root, &llm).await;
    daemon
        .reveal(&daemon.session, &["mob_create", "mob_spawn_member"])
        .await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    let outcome = client.turn_finished().await;
    assert_eq!(outcome.unwrap().text, "after the timeout");
    assert!(client.events().iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionTimedOut { name, .. } if name == "delegate"
    )));
    let connection = daemon.connect().await;
    until_empty(&connection, &format!("implicit-{}", daemon.session)).await;
    assert_eq!(members(&connection, "team").await, vec!["w1".to_string()]);
    client.quit().await;
    daemon.stop().await;
}

/// A helper's tool call is cut by the realm's tool deadline well before
/// the `delegate` that waits for the helper: with `[tools] default_timeout
/// = "10s"` and no `delegate` entry, the helper's `sleep` times out at
/// 10 s, the helper reports it, and the `delegate` call (waiting three
/// times as long, so 20 s are left for the helper's spawn and model calls
/// on a slow runner) completes with the helper's result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_s_tool_times_out_before_its_delegate_does() {
    let ws = Workspace::new();
    let config = common::realm_of(&ws.root).join("config.toml");
    std::fs::write(&config, "[tools]\ndefault_timeout = \"10s\"\n").unwrap();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let results = messages
            .iter()
            .filter(|m| matches!(m, Message::ToolResults { .. }))
            .count();
        if common::user_texts(messages).concat().contains("sleep long") {
            // The helper: one long command, then its report.
            return match results {
                0 => Step::Tool {
                    id: "s1",
                    name: "shell",
                    args: json!({"command": "sleep 300", "timeout_secs": 600}),
                },
                _ => Step::Text("the command timed out"),
            };
        }
        match results {
            0 => delegate("d1", "sleeper", "sleep long"),
            _ => Step::Text("the helper reported"),
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 10]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    let outcome = client.turn_finished().await;
    assert_eq!(outcome.unwrap().text, "the helper reported");
    let events = client.events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolExecutionCompleted { name, is_error: false, .. } if name == "delegate"
        )),
        "{events:?}"
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionTimedOut { name, .. } if name == "delegate"
    )));
    // The helper's own request saw its shell call time out.
    let helper_saw_timeout = llm.recorded().iter().any(|request| {
        request.user().contains("sleep long")
            && request.messages.iter().any(|m| match m {
                Message::ToolResults { results, .. } => results
                    .iter()
                    .any(|r| r.tool_use_id == "s1" && r.text_content().contains("timed out")),
                _ => false,
            })
    });
    assert!(helper_saw_timeout, "{:?}", llm.recorded());
    client.quit().await;
    daemon.stop().await;
}

/// What the persona says of a helper's mob holds: after a `delegate`,
/// the agent's `mob_list` names `implicit-<its session id>`, where the
/// helper was a member.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mob_list_shows_the_mob_of_the_agent_s_helpers() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        delegate("d1", "scout", "say hi"),
        Step::Text("hi"),
        Step::Tool {
            id: "l1",
            name: "mob_list",
            args: json!({}),
        },
        Step::Text("listed"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    daemon.reveal(&daemon.session, &["mob_list"]).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "listed");
    let listed = llm
        .recorded()
        .pop()
        .unwrap()
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResults { results, .. } => results
                .iter()
                .find(|r| r.tool_use_id == "l1")
                .map(|r| r.text_content()),
            _ => None,
        })
        .unwrap();
    assert!(
        listed.contains(&format!("implicit-{}", daemon.session)),
        "{listed}"
    );
    client.quit().await;
    daemon.stop().await;
}
