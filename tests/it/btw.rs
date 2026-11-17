//! `/btw <question>` end to end on the rig: a side question answered by one
//! model call of its own (the scripted model tells it from the agent's by
//! its system prompt) while the agent works, which leaves the agent's run,
//! session and next request untouched.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{Recorded, ScriptedLlmClient, Step, Workspace, user_texts};
use meerkat_core::{AgentEvent, Message};
use rig::Attached;
use rkat_tui::daemon::btw::SYSTEM;
use rkat_tui::ui::{Effect, Msg};
use serde_json::json;

/// Whether `messages` are the side call's (its system prompt).
fn side(messages: &[Message]) -> bool {
    messages
        .iter()
        .any(|m| matches!(m, Message::System(system) if system.content == SYSTEM))
}

fn side_requests(llm: &ScriptedLlmClient) -> Vec<Recorded> {
    llm.recorded()
        .into_iter()
        .filter(|r| side(&r.messages))
        .collect()
}

fn agent_requests(llm: &ScriptedLlmClient) -> Vec<Recorded> {
    llm.recorded()
        .into_iter()
        .filter(|r| !side(&r.messages))
        .collect()
}

/// Everything a request says, as text.
fn all_text(messages: &[Message]) -> String {
    format!("{messages:?}")
}

fn btw(question: &str, running_tool: Option<(&str, u64)>, current_turn: Option<&str>) -> Effect {
    Effect::Btw {
        question: question.into(),
        running_tool: running_tool.map(|(name, secs)| (name.to_string(), secs)),
        current_turn: current_turn.map(str::to_string),
    }
}

/// While the agent is inside a slow shell call, `/btw` is answered; the
/// side call has no tools and carries the conversation, the running
/// tool's line and the question; the agent's run goes on to its end, and
/// its next request and its session hold no trace of the question or the
/// answer; the side call's tokens count under the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_side_question_during_a_slow_tool_leaves_the_run_alone() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if side(messages) {
            Step::TextUsage("SIDE: the build is still running", 777)
        } else if messages
            .iter()
            .any(|m| matches!(m, Message::ToolResults { .. }))
        {
            Step::Text("finished")
        } else {
            Step::Tool {
                id: "slow",
                name: "shell",
                args: json!({ "command": "sleep 2; echo built", "timeout_secs": 30 }),
            }
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 6]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let session = rig.daemon.session.clone();
    rig.send(Effect::StartTurn("run the long build".into()));
    rig.wait_for(|msg| {
        matches!(msg, Msg::Agent(env)
            if matches!(&env.payload, AgentEvent::ToolExecutionStarted { id, .. } if id == "slow"))
    })
    .await;
    let before = rig.messages().await.len();
    // As the UI sends it: the running turn as its transcript shows it.
    rig.send(btw(
        "is the build done yet?",
        Some(("shell", 1)),
        Some("## user\nrun the long build\n\n## tool shell sleep 2; echo built (running)"),
    ));
    let answered = rig.wait_for(|msg| matches!(msg, Msg::BtwAnswered(_))).await;
    assert!(
        matches!(&answered, Msg::BtwAnswered(Ok(text)) if text == "SIDE: the build is still running"),
        "{answered:?}"
    );
    // Nothing was written to the session by the side question.
    assert_eq!(rig.messages().await.len(), before);
    let side = side_requests(&llm);
    assert_eq!(side.len(), 1);
    assert!(side[0].tools.is_empty(), "{:?}", side[0].tools);
    let asked = user_texts(&side[0].messages).join("\n");
    assert!(
        asked.contains("as the user's screen shows it>\n## user\nrun the long build"),
        "{asked}"
    );
    assert!(asked.contains("running the tool `shell`"), "{asked}");
    assert!(
        asked.contains("The user's side question: is the build done yet?"),
        "{asked}"
    );

    let done = rig.turn_finished().await.unwrap();
    assert_eq!(done.text, "finished");
    let agent = agent_requests(&llm);
    assert_eq!(agent.len(), 2, "the run made its two calls, no more");
    for request in &agent {
        let text = all_text(&request.messages);
        assert!(!text.contains("is the build done yet?"), "{text}");
        assert!(!text.contains("SIDE: the build"), "{text}");
    }
    assert!(
        all_text(&agent[1].messages).contains("built"),
        "the tool ran to its end"
    );
    let stored = all_text(&rig.messages().await);
    assert!(!stored.contains("is the build done yet?"));
    assert!(!stored.contains("SIDE: the build"));
    // Its tokens count under the session's usage (`/cost`).
    let record = rig
        .daemon
        .local()
        .core()
        .ledger
        .get(&rkat_tui::daemon::usage::Account::Session(session))
        .await;
    let input: u64 = record.cost.per_model.values().map(|t| t.input).sum();
    assert!(input >= 777, "{record:?}");
    rig.quit().await;
}

/// While the agent's model call is held, a second `/btw` is refused, and
/// cancelling the pending one (Esc) leaves the agent's run alone: it ends
/// with its answer, and the cancelled question is never answered. An idle
/// agent's side question is answered too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_side_question_is_one_at_a_time_and_cancels_alone() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let agent_gate = Arc::clone(&gate);
    let step = Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if side(messages) {
            if all_text(messages).contains("never answered") {
                Step::Hang
            } else {
                Step::Text("SIDE: idle answer")
            }
        } else {
            Step::Gated(
                Arc::clone(&agent_gate),
                Box::new(Step::Text("agent answer")),
            )
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 6]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("think hard".into()));
    llm.wait_call(0).await;
    rig.send(btw("never answered?", None, Some("## user\nthink hard")));
    llm.wait_call(1).await;
    rig.send(btw("a second one?", None, None));
    rig.wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("still waiting")))
        .await;
    assert_eq!(side_requests(&llm).len(), 1, "the second was not asked");
    rig.send(Effect::CancelBtw);
    gate.notify_one();
    let done = rig.turn_finished().await.unwrap();
    assert_eq!(done.text, "agent answer");
    assert_eq!(agent_requests(&llm).len(), 1);
    // Idle: asked and answered.
    rig.send(btw("what did it say?", None, None));
    let answered = rig.wait_for(|msg| matches!(msg, Msg::BtwAnswered(_))).await;
    assert!(
        matches!(&answered, Msg::BtwAnswered(Ok(text)) if text == "SIDE: idle answer"),
        "{answered:?}"
    );
    let last = side_requests(&llm).pop().unwrap();
    assert!(
        user_texts(&last.messages)
            .join("\n")
            .contains("agent answer")
    );
    // The cancelled one never answered (the refusal and the idle answer
    // are the only answers).
    assert_eq!(
        rig.seen
            .iter()
            .filter(|msg| matches!(msg, Msg::BtwAnswered(_)))
            .count(),
        1
    );
    rig.quit().await;
}
