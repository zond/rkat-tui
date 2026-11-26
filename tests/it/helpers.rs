//! Helpers: the daemon's agent starts one with `assign`, and the attached
//! TUI shows the helper (`[helper · model]`, its events nested, its
//! result, the running count, its usage in `/cost`), from the mobs' own
//! event streams. Esc stops the agent's run only.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;
use std::time::Instant;

use common::{MODEL, ScriptedLlmClient, Step, Workspace, members};
use meerkat_core::{AgentEvent, Message, StreamScopeFrame};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rig::{Client, Running};
use rkat_tui::ui::{App, Effect, Msg, StatusInfo, StatusUpdate, TurnFailure};
use serde_json::json;

use common::{after, assign, is_helper, told_finished};

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

/// The agent's side of the plain helper tests: it assigns `HELPER-TASK` to
/// `scout`, ends its run with "waiting", and answers the note that the
/// helper finished with "parent done". The helper's step is `helper`.
fn assigning(helper: Step) -> Arc<ScriptedLlmClient> {
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if is_helper(messages, "HELPER-TASK") {
            return helper.clone();
        }
        if told_finished(messages) {
            return Step::Text("parent done");
        }
        if after(messages, "a1") {
            return Step::Text("waiting");
        }
        assign("a1", "scout", "HELPER-TASK: look around")
    }));
    ScriptedLlmClient::new(vec![step; 12])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_assigned_helper_is_followed_with_its_result_and_cost() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = assigning(Step::Gated(
        Arc::clone(&gate),
        Box::new(Step::Text("HELPER-DONE")),
    ));
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("USER-ASK start a helper".into()));
    // The helper runs (its model call waits on the gate): the TUI lists it.
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| is_helper(&r.messages, "HELPER-TASK"))
    })
    .await;
    client.saw(one_helper("scout")).await;
    gate.notify_one();
    // Its answer reaches the agent as a note, which starts a run of it.
    client
        .saw(|m| matches!(m, Msg::Scoped(ev) if matches!(&ev.event, AgentEvent::TextComplete { content, .. } if content == "HELPER-DONE")))
        .await;
    client.saw(no_helpers).await;
    common::until(|| common::parent_told(&llm, "HELPER-TASK", "scout")).await;
    daemon.idle(&daemon.agent).await;
    client
        .saw(|m| matches!(m, Msg::Agent(env) if matches!(&env.payload, AgentEvent::TextComplete { content, .. } if content == "parent done")))
        .await;
    // The run's end: the usage of its call comes after its text, and the
    // cost below counts it.
    client
        .saw(|m| matches!(m, Msg::TurnFinished(Ok(summary)) if summary.text == "parent done"))
        .await;

    let recorded = llm.recorded();
    assert!(recorded[0].tools.iter().any(|t| t == "assign"));
    let helper = recorded
        .iter()
        .find(|r| is_helper(&r.messages, "HELPER-TASK"))
        .unwrap();
    assert_eq!(helper.model, MODEL);
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
        40,
        "three calls of the agent (the note's run is the third) and one of the helper"
    );
    // Its full view reads as the main transcript: the task the agent gave
    // it (from its run's own start), then its answer.
    use rkat_tui::ui::transcript::Block;
    let scout = app.transcript.helper_header("scout").unwrap();
    let view = app.transcript.helper_view_blocks(scout);
    match app.transcript.block(view[0]) {
        Some(Block::Task { label, text }) => {
            assert!(label.starts_with("task from main-"), "{label}");
            assert_eq!(text, "HELPER-TASK: look around");
        }
        other => panic!("the view starts with {other:?}"),
    }
    assert!(
        matches!(app.transcript.block(*view.last().unwrap()), Some(Block::Assistant { md }) if md == "HELPER-DONE"),
        "{view:?}"
    );
    client.quit().await;
    daemon.stop().await;
}

/// The number of tool results in `messages`.
fn results(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m, Message::ToolResults { .. }))
        .count()
}

/// A script where the agent assigns `HELPER-TASK` to `slow` on "go" and
/// is then held in a model call that never answers (its run stays open
/// for the user's interrupt): the helper's first model call waits on
/// `gate`, then calls `glob`, then answers `HELPER-ANSWER`; the agent
/// answers "after" to a message that says "again".
fn interrupted_assign(gate: &Arc<tokio::sync::Notify>) -> Arc<ScriptedLlmClient> {
    let gate = Arc::clone(gate);
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if is_helper(messages, "HELPER-TASK") {
            return match results(messages) {
                0 => Step::Gated(
                    Arc::clone(&gate),
                    Box::new(Step::Tool {
                        id: "g1",
                        name: "glob",
                        args: json!({"pattern": "*.nothing"}),
                    }),
                ),
                _ => Step::Text("HELPER-ANSWER"),
            };
        }
        if common::user_texts(messages).concat().contains("again") {
            return Step::Text("after");
        }
        if results(messages) == 0 {
            return assign("a1", "slow", "HELPER-TASK: look around");
        }
        Step::Hang
    }));
    ScriptedLlmClient::new(vec![step; 20])
}

/// The agent's model calls in `llm` (not its helper's).
fn agent_calls(llm: &ScriptedLlmClient) -> usize {
    llm.recorded()
        .iter()
        .filter(|r| !is_helper(&r.messages, "HELPER-TASK"))
        .count()
}

/// An interrupt stops the agent's run only (the owner's decision). The
/// agent's run is held open after its `assign` (its next
/// model call never answers), so the interrupt has a run to stop while the
/// helper runs. The helper keeps running, shown by its next model call
/// after the interrupt was confirmed; its answer is held for the agent its
/// user just interrupted (no run is started for it), the client is told at
/// once, and the user's next message carries it first. The interrupt does
/// not retire it: it is retired after it finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupt_leaves_the_helper_running_and_its_answer_comes_with_the_next_message() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = interrupted_assign(&gate);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    client.saw(one_helper("slow")).await;
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| is_helper(&r.messages, "HELPER-TASK"))
    })
    .await;
    // The agent's run is in its held call, after the `assign` returned.
    common::until(|| agent_calls(&llm) == 2).await;
    client.send(Effect::Interrupt);
    let outcome = client.turn_finished().await;
    assert_eq!(outcome, Err(TurnFailure::Interrupted), "{outcome:?}");
    // The interrupt is confirmed; the helper is still seated and goes on.
    let connection = daemon.connect().await;
    assert!(
        members(&connection, "tui")
            .await
            .contains(&"slow".to_string())
    );
    let before = llm.recorded().len();
    gate.notify_one();
    common::until(|| {
        llm.recorded()
            .iter()
            .skip(before)
            .any(|r| is_helper(&r.messages, "HELPER-TASK") && results(&r.messages) == 1)
    })
    .await;
    // It finished: the client is told how, and that the agent hears of it
    // with the next message; the agent was not woken.
    let ended = client
        .saw(|m| {
            matches!(m, Msg::HelperEnded { path, ending, .. }
            if path == &["slow".to_string()] && *ending != rkat_tui::daemon::protocol::HelperEnding::Retired)
        })
        .await;
    let Msg::HelperEnded { how, told, .. } = ended else {
        unreachable!()
    };
    assert_eq!(how, "finished");
    assert!(
        told.as_deref()
            .is_some_and(|t| t.contains("told with your next message")),
        "{told:?}"
    );
    common::until_async(async || {
        !members(&connection, "tui")
            .await
            .contains(&"slow".to_string())
    })
    .await;
    assert_eq!(agent_calls(&llm), 2, "no run was started for the agent");
    // The next message carries the answer, before the message itself.
    client.send(Effect::StartTurn("again".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "after");
    let request = llm
        .recorded()
        .into_iter()
        .rfind(|r| r.user().contains("again"))
        .unwrap();
    let message = request
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User(u) if u.text_content().contains("again") => Some(u.text_content()),
            _ => None,
        })
        .unwrap();
    let note = message
        .find("Your helper `slow` finished its task")
        .unwrap_or_else(|| panic!("the helper's ending travels with the message: {message}"));
    assert!(message[note..].contains("HELPER-ANSWER"), "{message}");
    assert!(
        note < message.rfind("again").unwrap(),
        "the note comes before the user's words: {message}"
    );
    // The classifier's record of the user's words has the user's only.
    let transcript = daemon.engine().transcript(&daemon.session).unwrap();
    assert!(
        transcript.contains("again") && !transcript.contains("HELPER-ANSWER"),
        "{transcript}"
    );
    client.quit().await;
    daemon.stop().await;
}

/// The adoption test of the lukacf/meerkat#2102 fix on the daemon: after a
/// `/model` switch the agent's `assign` seats its helper on the switched
/// model (the helper is given the session's model as it is now, not the
/// one the agent was spawned with).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_assigned_after_a_model_switch_runs_on_the_new_model() {
    const SWITCHED: &str = "claude-opus-4-8";
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let step = {
        let gate = Arc::clone(&gate);
        Step::Dynamic(Arc::new(move |messages: &[Message]| {
            if is_helper(messages, "say hi") {
                return Step::Gated(Arc::clone(&gate), Box::new(Step::Text("hi")));
            }
            if told_finished(messages) {
                return Step::Text("done");
            }
            if after(messages, "a1") {
                return Step::Text("waiting");
            }
            if common::user_texts(messages).concat().contains("start one") {
                return assign("a1", "scout", "say hi");
            }
            Step::Text("one")
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 12]);
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
    client.send(Effect::StartTurn("start one".into()));
    // The helper's session, while it runs (it is retired when done).
    let registry = daemon.registry();
    let key = rkat_tui::mobs::HelperKey::new(
        meerkat_mob::MobId::from("tui"),
        meerkat_mob::AgentIdentity::from("scout"),
    );
    let helper = common::poll(async || registry.member_session(&key)).await;
    gate.notify_one();
    common::until(|| common::parent_told(&llm, "say hi", "scout")).await;
    daemon.idle(&daemon.agent).await;
    let recorded = llm.recorded();
    assert_eq!(recorded[0].model, MODEL);
    assert_eq!(recorded[1].model, SWITCHED);
    let on = recorded
        .iter()
        .find(|r| is_helper(&r.messages, "say hi"))
        .unwrap();
    assert_eq!(on.model, SWITCHED, "the helper runs on the new model");
    client.quit().await;
    daemon.stop().await;
    let meta = rig::stored_session(&ws.root, &helper)
        .await
        .session_metadata()
        .unwrap();
    assert_eq!(meta.model, SWITCHED);
}

/// A helper whose own run fails is told to its agent as failed, with the
/// reason, and retired; nothing stays seated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_assigned_helper_whose_run_fails_is_told_and_leaves_no_helper_seated() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if is_helper(messages, "fail please") {
            // The helper's model call: a 400, which is not retried.
            return Step::RejectTools;
        }
        if common::user_texts(messages)
            .concat()
            .contains("Your helper `doomed` failed")
        {
            return Step::Text("after the failure");
        }
        if after(messages, "a1") {
            return Step::Text("waiting");
        }
        assign("a1", "doomed", "fail please")
    }));
    let llm = ScriptedLlmClient::new(vec![step; 10]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    client
        .saw(|m| matches!(m, Msg::Agent(env) if matches!(&env.payload, AgentEvent::TextComplete { content, .. } if content == "after the failure")))
        .await;
    let note = llm
        .recorded()
        .iter()
        .flat_map(|r| common::user_texts(&r.messages))
        .find(|text| text.contains("Your helper `doomed` failed"))
        .unwrap();
    assert!(note.contains("tools are not supported"), "{note}");
    let connection = daemon.connect().await;
    common::until_async(async || {
        !members(&connection, "tui")
            .await
            .contains(&"doomed".to_string())
    })
    .await;
    client.quit().await;
    daemon.stop().await;
}
