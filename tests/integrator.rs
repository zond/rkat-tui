//! The runner end to end, headless: effects in, UI messages out, through the
//! real runtime-backed service (`PersistentSessionService` + `MeerkatMachine`)
//! with a scripted LLM. Covers a turn, a queued turn, a steered message
//! applied at a tool boundary (and one typed while a slow shell call runs),
//! interrupt (also during a slow tool), resume by id and `last`, `-c`
//! across process lifetimes, and a renamed workspace.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::{Rig, RigOptions, ScriptedLlmClient, Step, Workspace, copy_tree, user_texts};
use meerkat_core::{AgentEvent, Message};
use rkat_tui::approval::classifier::{
    ActionClassifier, ClassifierError, ClassifierInput, ClassifierVerdict,
};
use rkat_tui::bootstrap::ApprovalOverrides;
use rkat_tui::delivery::PendingId;
use rkat_tui::runner::Start;
use rkat_tui::session::resolve_session_ref;
use rkat_tui::ui::{Block, Effect, EscalationAnswer, Msg, StatusUpdate, TurnFailure};
use serde_json::json;
use tokio::sync::Notify;

fn steer(id: u64, text: &str) -> Effect {
    Effect::Steer {
        id: PendingId(id),
        text: text.into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creates_a_session_and_completes_a_turn() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("hello there")]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    // The workspace-local realm lives inside the workspace.
    assert!(ws.root.join(".rkat/tui/.gitignore").is_file());

    rig.send(Effect::StartTurn("hi".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.expect("turn succeeds").text, "hello there");
    assert!(outcome.undelivered.is_empty());

    let session = rig.session(&rig.first_session).await;
    assert_eq!(user_texts(session.messages()), vec!["hi".to_string()]);
    // The persona reached the model.
    let first = &llm.requests()[0];
    assert!(first.iter().any(|m| matches!(
        m,
        Message::System(system) if system.content.contains("interactive coding agent")
    )));
    assert!(rig.quit().await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_queued_behind_a_running_turn_runs_next() {
    let ws = Workspace::new();
    let gate = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&gate), Box::new(Step::Text("first answer"))),
        Step::Text("second answer"),
    ]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    rig.send(Effect::StartTurn("one".into()));
    llm.wait_call(0).await;
    // A second prompt while the first runs: a `Queue` input behind it.
    rig.send(Effect::StartTurn("two".into()));
    gate.notify_one();

    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.expect("turn succeeds").text, "second answer");
    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        user_texts(&requests[1]),
        vec!["one".to_string(), "two".to_string()]
    );
    let session = rig.session(&rig.first_session).await;
    assert_eq!(
        user_texts(session.messages()),
        vec!["one".to_string(), "two".to_string()]
    );
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steered_message_is_applied_at_the_next_tool_boundary() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("notes.txt"), "the answer is 42\n").unwrap();
    let gate = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(
            Arc::clone(&gate),
            Box::new(Step::Tool {
                id: "call-1",
                name: "read_file",
                args: json!({ "path": "notes.txt" }),
            }),
        ),
        Step::Text("done"),
    ]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    rig.send(Effect::StartTurn("read the notes".into()));
    llm.wait_call(0).await;
    // Typed while the model streams: steered into the running turn.
    rig.send(steer(1, "also mention the file name"));
    let ack = rig
        .wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    assert!(
        matches!(
            ack,
            Msg::SteerAck {
                id: PendingId(1),
                result: Ok(())
            }
        ),
        "{ack:?}"
    );
    gate.notify_one();

    rig.wait_for(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))))
        .await;
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.expect("turn succeeds").text, "done");
    assert!(outcome.undelivered.is_empty());

    // One run: the steer joined it at the tool boundary, after the tool
    // result (the file tool ran end to end) and before the next request.
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "no follow-up turn");
    let second = &requests[1];
    let tool_result_at = second
        .iter()
        .position(|m| {
            matches!(m, Message::ToolResults { results, .. }
            if results.iter().any(|r| r.text_content().contains("the answer is 42")))
        })
        .expect("the read_file result reached the model");
    let steer_at = second
        .iter()
        .position(
            |m| matches!(m, Message::User(u) if u.text_content() == "also mention the file name"),
        )
        .expect("the steered message reached the model");
    assert!(steer_at > tool_result_at);
    assert!(
        rig.events
            .lock()
            .iter()
            .any(|event| matches!(event, AgentEvent::BoundaryAppendApplied { .. }))
    );
    let session = rig.session(&rig.first_session).await;
    assert_eq!(
        user_texts(session.messages()),
        vec![
            "read the notes".to_string(),
            "also mention the file name".to_string()
        ]
    );
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_cancels_the_running_turn_and_returns_steers() {
    let ws = Workspace::new();
    // The first response never arrives.
    let never = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(never, Box::new(Step::Text("never"))),
        Step::Text("after the interrupt"),
    ]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    rig.send(Effect::StartTurn("long task".into()));
    llm.wait_call(0).await;
    rig.send(steer(1, "and this"));
    rig.wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    rig.send(Effect::Interrupt);

    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result, Err(TurnFailure::Interrupted));
    assert_eq!(outcome.undelivered, vec![PendingId(1)]);

    // The session stays usable, and the cancelled steer never runs.
    rig.send(Effect::StartTurn("next".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(
        outcome.result.expect("turn succeeds").text,
        "after the interrupt"
    );
    assert!(
        !user_texts(llm.requests().last().unwrap()).contains(&"and this".to_string()),
        "a cancelled steer must not reach the model"
    );
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_and_resume_switch_sessions_in_process() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Text("first"),
        Step::Text("second"),
        Step::Text("third"),
    ]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    let first = rig.first_session.clone();
    rig.send(Effect::StartTurn("in the first session".into()));
    rig.turn_finished().await;

    rig.send(Effect::NewSession);
    let second = match rig
        .wait_for(|msg| matches!(msg, Msg::Status(StatusUpdate::Session(_))))
        .await
    {
        Msg::Status(StatusUpdate::Session(id)) => id,
        other => panic!("{other:?}"),
    };
    assert_ne!(second, first.to_string());
    rig.send(Effect::StartTurn("in the second session".into()));
    rig.turn_finished().await;
    assert_eq!(
        user_texts(llm.requests().last().unwrap()),
        vec!["in the second session".to_string()]
    );

    // Resume the first by id: the transcript comes back, and the next turn
    // continues its history.
    rig.send(Effect::ResumeSession(first.to_string()));
    let blocks = match rig
        .wait_for(|msg| matches!(msg, Msg::ResetTranscript(_)))
        .await
    {
        Msg::ResetTranscript(blocks) => blocks,
        other => panic!("{other:?}"),
    };
    assert!(blocks.contains(&Block::User {
        text: "in the first session".into()
    }));
    rig.send(Effect::StartTurn("back again".into()));
    rig.turn_finished().await;
    assert_eq!(
        user_texts(llm.requests().last().unwrap()),
        vec!["in the first session".to_string(), "back again".to_string()]
    );
    // `last` is the session updated most recently.
    let last = resolve_session_ref(&*rig.store, "last").await.unwrap();
    assert_eq!(last, first);
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continue_and_resume_by_id_across_lifetimes() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("remembered")]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    let first = rig.first_session.clone();
    rig.send(Effect::StartTurn("remember me".into()));
    rig.turn_finished().await;
    rig.quit().await;

    // `-c`: the workspace's most recent session, with its transcript.
    let llm = ScriptedLlmClient::new(vec![Step::Text("continued")]);
    let mut rig = Rig::start(&ws.root, &llm, Start::Resume("last".into())).await;
    assert_eq!(rig.first_session, first);
    let blocks = match rig
        .wait_for(|msg| matches!(msg, Msg::ResetTranscript(_)))
        .await
    {
        Msg::ResetTranscript(blocks) => blocks,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        blocks,
        vec![
            Block::User {
                text: "remember me".into()
            },
            Block::Assistant {
                md: "remembered".into()
            },
        ]
    );
    rig.send(Effect::StartTurn("still there?".into()));
    rig.turn_finished().await;
    assert_eq!(
        user_texts(&llm.requests()[0]),
        vec!["remember me".to_string(), "still there?".to_string()]
    );
    rig.quit().await;

    // `--resume <id prefix>`.
    let prefix = first.to_string()[..8].to_string();
    let llm = ScriptedLlmClient::new(vec![]);
    let rig = Rig::start(&ws.root, &llm, Start::Resume(prefix)).await;
    assert_eq!(rig.first_session, first);
    rig.quit().await;
}

/// `--resume` without a reference: a fresh session, and the picker offers
/// the earlier one first thing; choosing it resumes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_without_a_reference_opens_the_picker_at_start() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    let first = rig.first_session.clone();
    rig.send(Effect::StartTurn("pick me later".into()));
    rig.turn_finished().await;
    rig.quit().await;

    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Rig::start(&ws.root, &llm, Start::Pick).await;
    assert_ne!(rig.first_session, first);
    let items = match rig
        .wait_for(|msg| matches!(msg, Msg::ShowResumePicker(_)))
        .await
    {
        Msg::ShowResumePicker(items) => items,
        other => panic!("{other:?}"),
    };
    let first_id = first.to_string();
    let chosen = items
        .iter()
        .find(|(reference, _)| reference == &first_id)
        .map(|(reference, _)| reference.clone())
        .unwrap_or_else(|| panic!("the earlier session is offered: {items:?}"));
    rig.send(Effect::ResumeSession(chosen));
    let blocks = match rig
        .wait_for(|msg| matches!(msg, Msg::ResetTranscript(blocks) if !blocks.is_empty()))
        .await
    {
        Msg::ResetTranscript(blocks) => blocks,
        other => panic!("{other:?}"),
    };
    assert!(blocks.contains(&Block::User {
        text: "pick me later".into()
    }));
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_state_survives_a_rename() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    let first = rig.first_session.clone();
    rig.send(Effect::StartTurn("before the move".into()));
    rig.turn_finished().await;
    rig.quit().await;

    let moved = ws.sibling("renamed-project");
    copy_tree(&ws.root, &moved);
    std::fs::remove_dir_all(&ws.root).unwrap();

    let llm = ScriptedLlmClient::new(vec![]);
    let rig = Rig::start(&moved, &llm, Start::Resume("last".into())).await;
    assert_eq!(rig.first_session, first);
    let session = rig.session(&first).await;
    assert_eq!(
        user_texts(session.messages()),
        vec!["before the move".to_string()]
    );
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_without_a_running_turn_is_refused() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    // A steer without a running turn is refused so the UI sends it as a turn.
    rig.send(steer(7, "idle"));
    let msg = rig
        .wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    assert!(
        matches!(
            msg,
            Msg::SteerAck {
                id: PendingId(7),
                result: Err(rkat_tui::delivery::SteerFailure::NotRunning)
            }
        ),
        "{msg:?}"
    );
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_builtins_the_tui_owns_are_denied_by_the_tool_policy() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "swap",
            name: "brain_swap",
            args: json!({ "model": "claude-opus-4-8" }),
        },
        Step::Tool {
            id: "patch",
            name: "apply_patch",
            args: json!({ "input": "*** Begin Patch\n*** End Patch\n" }),
        },
        Step::Text("done"),
    ]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    rig.send(Effect::StartTurn("try them".into()));
    rig.turn_finished().await;
    let requests = llm.requests();
    let result_of = |id: &str| {
        requests
            .iter()
            .flatten()
            .find_map(|m| match m {
                Message::ToolResults { results, .. } => {
                    results.iter().find(|r| r.tool_use_id == id).cloned()
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no result for {id}"))
    };
    for id in ["swap", "patch"] {
        let result = result_of(id);
        assert!(result.is_error, "{id}: {result:?}");
    }
    // The model never changed.
    let meta = rig
        .session(&rig.first_session)
        .await
        .session_metadata()
        .unwrap();
    assert_eq!(meta.model, common::MODEL);
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_prompt_after_a_failed_turn_is_not_added_twice() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::RejectTools, Step::Text("fixed")]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    rig.send(Effect::StartTurn("fix the build".into()));
    let outcome = rig.turn_finished().await;
    assert!(
        matches!(outcome.result, Err(TurnFailure::Failed(_))),
        "{:?}",
        outcome.result
    );
    // The failed run committed the prompt.
    let session = rig.session(&rig.first_session).await;
    assert_eq!(
        user_texts(session.messages()),
        vec!["fix the build".to_string()]
    );

    // Re-typed (with stray whitespace): a retry, not a second copy.
    rig.send(Effect::StartTurn("  fix the build\n".into()));
    rig.wait_for(
        |msg| matches!(msg, Msg::Notice { text, .. } if text.contains("already in the transcript")),
    )
    .await;
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.expect("retry succeeds").text, "fixed");
    let expected = vec![
        "fix the build".to_string(),
        "(retrying the request above)".to_string(),
    ];
    assert_eq!(user_texts(&llm.requests()[1]), expected);
    let session = rig.session(&rig.first_session).await;
    assert_eq!(user_texts(session.messages()), expected);
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_different_prompt_after_a_failed_turn_is_sent_as_written() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::RejectTools,
        Step::Text("first"),
        Step::Text("second"),
    ]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    rig.send(Effect::StartTurn("fix the build".into()));
    assert!(rig.turn_finished().await.result.is_err());

    rig.send(Effect::StartTurn("run the tests instead".into()));
    assert_eq!(
        rig.turn_finished().await.result.expect("turn").text,
        "first"
    );
    // Only right after the failure: a repeat after a success is a message.
    rig.send(Effect::StartTurn("run the tests instead".into()));
    assert_eq!(
        rig.turn_finished().await.result.expect("turn").text,
        "second"
    );
    let session = rig.session(&rig.first_session).await;
    assert_eq!(
        user_texts(session.messages()),
        vec![
            "fix the build".to_string(),
            "run the tests instead".to_string(),
            "run the tests instead".to_string(),
        ]
    );
    rig.quit().await;
}

/// Allows every reviewed call: the slow-tool cases are about delivery, not
/// the approval gate.
struct AllowAll;

#[async_trait]
impl ActionClassifier for AllowAll {
    async fn classify(
        &self,
        _input: ClassifierInput<'_>,
    ) -> Result<ClassifierVerdict, ClassifierError> {
        Ok(ClassifierVerdict::allow("ok"))
    }
}

fn allow_shell() -> RigOptions {
    RigOptions {
        approval: ApprovalOverrides {
            classifier: Some(Arc::new(AllowAll)),
            ..ApprovalOverrides::default()
        },
        ..RigOptions::default()
    }
}

/// The dogfood case: a message typed while the model's shell call runs
/// reaches the model after that call's result, and the call runs to its end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_typed_while_a_slow_tool_runs_reaches_the_model_after_its_result() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "slow",
            name: "shell",
            args: json!({ "command": "sleep 2; echo done sleeping", "timeout_secs": 30 }),
        },
        Step::Text("finished"),
        Step::Text("follow-up answer"),
    ]);
    let mut rig = Rig::start_opts(&ws.root, &llm, Start::New, allow_shell()).await;
    rig.send(Effect::StartTurn("do something for a while".into()));
    tool_started(&rig, "slow").await;
    rig.send(steer(1, "typed while it sleeps"));
    let ack = rig
        .wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    assert!(
        matches!(ack, Msg::SteerAck { result: Ok(()), .. }),
        "{ack:?}"
    );

    rig.wait_for(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))))
        .await;
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.expect("turn succeeds").text, "finished");
    assert!(outcome.undelivered.is_empty());
    // One run: the shell ran to its end, its result reached the model, and
    // the typed message followed it in the same request.
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "no follow-up turn");
    let second = &requests[1];
    let result_at = second
        .iter()
        .position(|m| {
            matches!(m, Message::ToolResults { results, .. }
            if results.iter().any(|r| !r.is_error && r.text_content().contains("done sleeping")))
        })
        .expect("the shell result reached the model");
    let typed_at = second
        .iter()
        .position(|m| matches!(m, Message::User(u) if u.text_content() == "typed while it sleeps"))
        .expect("the typed message reached the model");
    assert!(typed_at > result_at);
    let session = rig.session(&rig.first_session).await;
    assert!(
        !session
            .messages()
            .iter()
            .any(|m| matches!(m, Message::SystemNotice(_))),
        "nothing was closed by recovery"
    );
    rig.quit().await;
}

/// Allows every reviewed call with a stated reason.
struct AllowBecause(&'static str);

#[async_trait]
impl ActionClassifier for AllowBecause {
    async fn classify(
        &self,
        _input: ClassifierInput<'_>,
    ) -> Result<ClassifierVerdict, ClassifierError> {
        Ok(ClassifierVerdict::allow(self.0))
    }
}

/// A call the classifier allowed gets a dim note under its tool block with
/// the classifier's reason; a tier-1 call gets none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_classifier_allowed_call_gets_a_note_and_a_tier1_call_does_not() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("scratch.txt"), "x").unwrap();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tools(vec![
            ("tier1", "shell", json!({ "command": "ls" })),
            (
                "reviewed",
                "shell",
                json!({ "command": "rm -rf scratch.txt" }),
            ),
        ]),
        Step::Text("cleaned up"),
    ]);
    let options = RigOptions {
        approval: ApprovalOverrides {
            classifier: Some(Arc::new(AllowBecause(
                "removes an in-project scratch file\nthe user asked to clean up",
            ))),
            ..ApprovalOverrides::default()
        },
        ..RigOptions::default()
    };
    let mut rig = Rig::start_opts(&ws.root, &llm, Start::New, options).await;
    rig.send(Effect::StartTurn("clean up the scratch file".into()));
    let note = rig
        .wait_for(
            |msg| matches!(msg, Msg::ToolNote { tool_use_id, .. } if tool_use_id == "reviewed"),
        )
        .await;
    let Msg::ToolNote { text, .. } = note else {
        panic!("not a note: {note:?}");
    };
    assert_eq!(
        text,
        "allowed by classifier: removes an in-project scratch file the user asked to clean up"
    );
    assert_eq!(
        rig.turn_finished().await.result.expect("turn").text,
        "cleaned up"
    );
    assert!(
        !ws.root.join("scratch.txt").exists(),
        "the reviewed call ran"
    );
    let notes: Vec<&Msg> = rig
        .seen
        .iter()
        .filter(|msg| matches!(msg, Msg::ToolNote { .. }))
        .collect();
    assert_eq!(notes.len(), 1, "only the reviewed call is noted: {notes:?}");
    // The classifier allow no longer logs a separate approval line.
    assert!(!rig.seen.iter().any(|msg| matches!(msg, Msg::Approval(_))));
    rig.quit().await;
}

/// The model writes a protected `.env.test`: even with a classifier that
/// allows everything, the UI gets an escalation naming the protected path;
/// answering allow runs the write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_protected_write_is_put_to_the_user_and_runs_when_allowed() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "env",
            name: "write_file",
            args: json!({ "path": ".env.test", "content": "API_URL=http://localhost\n" }),
        },
        Step::Text("configured"),
    ]);
    let mut rig = Rig::start_opts(&ws.root, &llm, Start::New, allow_shell()).await;
    rig.send(Effect::StartTurn("add a test env file".into()));
    let Msg::Escalation(prompt, id) = rig.wait_for(|msg| matches!(msg, Msg::Escalation(..))).await
    else {
        unreachable!()
    };
    assert_eq!(prompt.tool, "write_file");
    assert!(
        prompt
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("protected path: `.env.test` (secrets)")),
        "{prompt:?}"
    );
    rig.send(Effect::RespondEscalation(id, EscalationAnswer::AllowOnce));
    assert_eq!(
        rig.turn_finished().await.result.expect("turn").text,
        "configured"
    );
    assert_eq!(
        std::fs::read_to_string(ws.root.join(".env.test")).unwrap(),
        "API_URL=http://localhost\n"
    );
    assert!(
        !rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::ToolNote { .. })),
        "no classifier note"
    );
    rig.quit().await;
}

/// Waits until the tool call `id` has started executing.
async fn tool_started(rig: &Rig, id: &str) {
    common::within(async {
        loop {
            let started = rig.events.lock().iter().any(|event| {
                matches!(event, AgentEvent::ToolExecutionStarted { id: started, .. } if started == id)
            });
            if started {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupting_a_slow_tool_returns_the_messages_typed_meanwhile() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "slow",
            name: "shell",
            args: json!({ "command": "sleep 20; echo done sleeping", "timeout_secs": 30 }),
        },
        Step::Text("after the interrupt"),
    ]);
    let mut rig = Rig::start_opts(&ws.root, &llm, Start::New, allow_shell()).await;
    rig.send(Effect::StartTurn("do something for a while".into()));
    tool_started(&rig, "slow").await;
    rig.send(steer(1, "Test"));
    rig.wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    rig.send(steer(2, "Hepp"));
    rig.wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    rig.send(Effect::Interrupt);
    let outcome = rig.turn_finished().await;
    // Interrupted (not "failed: interrupted by the user"), and both typed
    // messages come back: neither ran as a follow-up turn.
    assert_eq!(outcome.result, Err(TurnFailure::Interrupted));
    assert_eq!(outcome.undelivered, vec![PendingId(1), PendingId(2)]);
    assert_eq!(
        llm.requests().len(),
        1,
        "no follow-up turn after the interrupt"
    );
    // The interrupt closed the run through recovery: the call is answered
    // as outcome unknown and the notice says the input was not requeued.
    let session = rig.session(&rig.first_session).await;
    assert!(session.messages().iter().any(|m| matches!(
        m,
        Message::ToolResults { results, .. }
            if results.iter().any(|r| r.is_error && r.text_content().contains("outcome_uncertain"))
    )));
    assert!(
        !user_texts(session.messages())
            .iter()
            .any(|t| t == "Test" || t == "Hepp")
    );
    // The session stays usable.
    rig.send(Effect::StartTurn("Test\n\nHepp".into()));
    assert_eq!(
        rig.turn_finished().await.result.expect("turn").text,
        "after the interrupt"
    );
    rig.quit().await;
}

/// Esc with a message typed during a slow tool: the message is pulled back
/// (it never reaches the model) and the turn runs on: the tool ends, its
/// result reaches the model, the turn completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recalling_a_message_typed_during_a_slow_tool_leaves_the_run_alone() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "slow",
            name: "shell",
            args: json!({ "command": "sleep 2; echo done sleeping", "timeout_secs": 30 }),
        },
        Step::Text("finished"),
        Step::Text("follow-up answer"),
    ]);
    let mut rig = Rig::start_opts(&ws.root, &llm, Start::New, allow_shell()).await;
    rig.send(Effect::StartTurn("do something for a while".into()));
    tool_started(&rig, "slow").await;
    rig.send(steer(1, "never mind"));
    let ack = rig
        .wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    assert!(
        matches!(ack, Msg::SteerAck { result: Ok(()), .. }),
        "{ack:?}"
    );
    rig.send(Effect::RecallQueued);
    let recalled = rig
        .wait_for(|msg| matches!(msg, Msg::Recalled { .. }))
        .await;
    assert!(
        matches!(&recalled, Msg::Recalled { recalled, errors }
            if recalled == &[PendingId(1)] && errors.is_empty()),
        "{recalled:?}"
    );

    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.expect("turn succeeds").text, "finished");
    assert!(outcome.undelivered.is_empty(), "already back in the editor");
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "no follow-up turn");
    assert!(
        requests[1]
            .iter()
            .any(|m| matches!(m, Message::ToolResults { results, .. }
        if results.iter().any(|r| !r.is_error && r.text_content().contains("done sleeping"))))
    );
    for request in &requests {
        assert!(!user_texts(request).contains(&"never mind".to_string()));
    }
    let session = rig.session(&rig.first_session).await;
    assert!(!user_texts(session.messages()).contains(&"never mind".to_string()));
    assert!(
        !session
            .messages()
            .iter()
            .any(|m| matches!(m, Message::SystemNotice(_))),
        "nothing was closed by recovery"
    );
    rig.quit().await;
}

/// A recall after the steer joined the run finds nothing to pull back: the
/// turn runs on with the steer delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recalling_a_steer_that_already_joined_the_run_leaves_it_delivered() {
    let ws = Workspace::new();
    let gate = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "slow",
            name: "shell",
            args: json!({ "command": "sleep 1; echo done sleeping", "timeout_secs": 30 }),
        },
        Step::Gated(Arc::clone(&gate), Box::new(Step::Text("finished"))),
    ]);
    let mut rig = Rig::start_opts(&ws.root, &llm, Start::New, allow_shell()).await;
    rig.send(Effect::StartTurn("do something for a while".into()));
    tool_started(&rig, "slow").await;
    rig.send(steer(1, "typed while it sleeps"));
    rig.wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    rig.wait_for(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))))
        .await;
    llm.wait_call(1).await;
    rig.send(Effect::RecallQueued);
    let recalled = rig
        .wait_for(|msg| matches!(msg, Msg::Recalled { .. }))
        .await;
    assert!(
        matches!(&recalled, Msg::Recalled { recalled, errors }
            if recalled.is_empty() && errors.is_empty()),
        "{recalled:?}"
    );
    gate.notify_one();

    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.expect("turn succeeds").text, "finished");
    assert!(outcome.undelivered.is_empty());
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "no follow-up turn");
    assert!(user_texts(&requests[1]).contains(&"typed while it sleeps".to_string()));
    rig.quit().await;
}

/// Feed the runner's messages to `app` (forwarding the effects it answers
/// with) until one matches `until`; returns that message.
async fn pump(rig: &mut Rig, app: &mut rkat_tui::ui::App, until: impl Fn(&Msg) -> bool) -> Msg {
    loop {
        let msg = rig.wait_for(|_| true).await;
        let stop = until(&msg);
        for effect in app.update(msg.clone()) {
            rig.send(effect);
        }
        if stop {
            return msg;
        }
    }
}

/// Type `text` into `app` and press `code`; forward the effects.
fn press(
    rig: &Rig,
    app: &mut rkat_tui::ui::App,
    text: &str,
    code: ratatui::crossterm::event::KeyCode,
) {
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    for c in text.chars() {
        app.update(Msg::Key(KeyEvent::from(KeyCode::Char(c))));
    }
    for effect in app.update(Msg::Key(KeyEvent::from(code))) {
        rig.send(effect);
    }
}

/// Esc while a turn runs interrupts it, and the message queued behind it
/// starts the next turn at once (Claude Code's behaviour), with the real UI
/// state machine deciding.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn esc_interrupts_and_the_queued_message_runs_next() {
    use ratatui::crossterm::event::KeyCode;
    let ws = Workspace::new();
    let never = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(never, Box::new(Step::Text("never"))),
        Step::Text("answered the queued message"),
    ]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    let mut app = rkat_tui::ui::App::new(rkat_tui::ui::StatusInfo::default());
    press(&rig, &mut app, "long task", KeyCode::Enter);
    llm.wait_call(0).await;
    press(&rig, &mut app, "and this", KeyCode::Enter);
    pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::SteerAck { .. })
    })
    .await;
    press(&rig, &mut app, "", KeyCode::Esc);
    let first = pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::TurnFinished(_))
    })
    .await;
    assert!(
        matches!(&first, Msg::TurnFinished(outcome)
            if outcome.result == Err(TurnFailure::Interrupted)
                && outcome.undelivered == vec![PendingId(1)]),
        "{first:?}"
    );
    // The UI started the next turn with the queued message on its own.
    assert!(app.is_running());
    let second = pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::TurnFinished(_))
    })
    .await;
    assert!(
        matches!(&second, Msg::TurnFinished(outcome)
            if outcome.result.as_ref().is_ok_and(|s| s.text == "answered the queued message")),
        "{second:?}"
    );
    assert!(user_texts(llm.requests().last().unwrap()).contains(&"and this".to_string()));
    assert!(app.input.is_empty());
    assert_eq!(app.status.queued, 0);
    rig.quit().await;
}

/// `/context`: the runner adds the session's system prompt, its messages,
/// the instruction files and the TUI's own tools to the UI's header.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_report_describes_the_session() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("AGENTS.md"), "# AGENTS.md\n\n- use tabs\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("hello")]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    rig.send(Effect::StartTurn("hi".into()));
    rig.turn_finished().await;
    rig.send(Effect::ContextReport("context: header".into()));
    let msg = rig
        .wait_for(
            |msg| matches!(msg, Msg::Notice { text, .. } if text.starts_with("context: header")),
        )
        .await;
    let Msg::Notice { text, .. } = msg else {
        unreachable!()
    };
    assert!(text.contains("system prompt: "), "{text}");
    assert!(text.contains("messages: 2 (user 1, assistant 1"), "{text}");
    assert!(text.contains("workspace AGENTS.md: "), "{text}");
    assert!(text.contains("24 bytes"), "{text}");
    assert!(text.contains("registered by rkat-tui"), "{text}");
    rig.quit().await;
}

/// `/status`: one notice naming the version, model, session and the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_report_names_the_session_and_the_workspace() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    rig.send(Effect::StatusReport);
    let msg = rig
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.starts_with("rkat-tui ")))
        .await;
    let Msg::Notice { text, .. } = msg else {
        unreachable!()
    };
    assert!(
        text.contains(&format!("session: {}", rig.first_session)),
        "{text}"
    );
    assert!(
        text.contains(&format!("workspace: {}", ws.root.display())),
        "{text}"
    );
    assert!(text.contains("shell confinement: "), "{text}");
    assert!(text.contains("auto mode: "), "{text}");
    assert!(text.contains("tui.toml: "), "{text}");
    assert!(text.contains("mcp.toml: absent"), "{text}");
    assert!(text.contains("helpers: "), "{text}");
    rig.quit().await;
}

/// `/export`: the runner writes the UI's text to the default file under the
/// workspace's `.rkat/tui/` (and to a typed path), and says where.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_writes_the_transcript_and_names_the_file() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    rig.send(Effect::Export {
        path: None,
        text: "## user\nhi\n".into(),
    });
    let msg = rig
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("export")))
        .await;
    let Msg::Notice { text, .. } = msg else {
        unreachable!()
    };
    let path = text
        .strip_prefix("exported the transcript to ")
        .unwrap_or_else(|| panic!("{text}"));
    let path = std::path::Path::new(path);
    assert!(path.starts_with(ws.root.join(".rkat/tui")), "{text}");
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with(&format!("export-{}-", rig.first_session)) && name.ends_with(".txt"),
        "{name}"
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "## user\nhi\n");

    rig.send(Effect::Export {
        path: Some("out/chat.txt".into()),
        text: "x\n".into(),
    });
    rig.wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("chat.txt")))
        .await;
    assert_eq!(
        std::fs::read_to_string(ws.root.join("out/chat.txt")).unwrap(),
        "x\n"
    );
    rig.quit().await;
}

/// The leases of the workspace realm `runtime` has open.
async fn realm_leases(runtime: &rkat_tui::bootstrap::Runtime) -> meerkat_store::RealmLeaseStatus {
    meerkat_store::inspect_realm_leases_in(
        &runtime.scope.state_root,
        &runtime.scope.realm_id,
        false,
    )
    .await
    .unwrap()
}

/// Every open realm holds a lease (what `rkat realm prune` and realm
/// inspection read), one per process: two opens of the same realm hold two,
/// each released on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_open_of_a_realm_holds_its_own_lease_until_released() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let open = || async {
        rkat_tui::bootstrap::bootstrap(
            &common::args_for(&ws.root),
            rkat_tui::bootstrap::BootstrapOptions {
                llm_override: Some(Arc::clone(&llm) as Arc<dyn meerkat::LlmClient>),
                lister: Some(common::no_lists()),
                host_paths: Some(rkat_tui::bootstrap::HostPaths::default()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    let mut first = common::within(open()).await;
    let mut second = common::within(open()).await;
    assert_eq!(first.scope.realm_id, second.scope.realm_id);

    let status = realm_leases(&first).await;
    assert_eq!(status.active.len(), 2, "{status:?}");
    assert!(status.active.iter().all(|lease| {
        lease.surface == rkat_tui::bootstrap::LEASE_SURFACE
            && lease.pid == std::process::id()
            && lease.realm_id == first.scope.realm_id
    }));
    assert!(status.blocks_destructive_prune());

    first.release_realm_lease().await;
    first.release_realm_lease().await;
    let status = realm_leases(&second).await;
    assert_eq!(status.active.len(), 1, "{status:?}");

    second.release_realm_lease().await;
    let status = realm_leases(&second).await;
    assert!(
        status.active.is_empty() && status.stale.is_empty(),
        "{status:?}"
    );
    assert!(!status.blocks_destructive_prune());
}

/// The runner's quit releases the lease the bootstrap took.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quitting_releases_the_realm_lease() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let rig = Rig::start(&ws.root, &llm, Start::New).await;
    // The workspace-local realm lives inside the workspace.
    assert_eq!(walk_leases(&ws.root), 1);
    rig.quit().await;
    assert_eq!(walk_leases(&ws.root), 0);
}

/// Lease records (`<realm>/leases/*.json`) under `dir`.
fn walk_leases(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                walk_leases(&path)
            } else {
                usize::from(
                    path.extension().is_some_and(|ext| ext == "json")
                        && path
                            .parent()
                            .and_then(|p| p.file_name())
                            .is_some_and(|n| n == "leases"),
                )
            }
        })
        .sum()
}

/// Session references resolve over the service's listing
/// (`SessionService::list`): an archived session is no longer `last`, `~N`
/// or a prefix match.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn references_resolve_over_the_service_listing() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("first"), Step::Text("second")]);
    let mut rig = Rig::start(&ws.root, &llm, Start::New).await;
    let first = rig.first_session.clone();
    rig.send(Effect::StartTurn("in the first session".into()));
    rig.turn_finished().await;
    rig.send(Effect::NewSession);
    let second = match rig
        .wait_for(|msg| matches!(msg, Msg::Status(StatusUpdate::Session(_))))
        .await
    {
        Msg::Status(StatusUpdate::Session(id)) => id,
        other => panic!("{other:?}"),
    };
    rig.send(Effect::StartTurn("in the second session".into()));
    rig.turn_finished().await;

    let listed = rkat_tui::session::list_sessions(&*rig.store).await.unwrap();
    let ids: Vec<String> = listed.iter().map(|s| s.session_id.to_string()).collect();
    assert_eq!(ids, vec![second.clone(), first.to_string()]);
    assert_eq!(resolve_session_ref(&*rig.store, "~1").await.unwrap(), first);
    // (v7 ids of one test share their leading timestamp digits.)
    let prefix = &first.to_string()[..30];
    assert_eq!(
        resolve_session_ref(&*rig.store, prefix).await.unwrap(),
        first
    );

    rig.service
        .archive_with_machine_protocol(
            &first,
            meerkat::MachineSessionArchiveProtocol::from_machine(rig.machine.as_ref()),
        )
        .await
        .unwrap();
    let listed = rkat_tui::session::list_sessions(&*rig.store).await.unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].session_id.to_string(), second);
    assert!(resolve_session_ref(&*rig.store, "~1").await.is_err());
    assert!(resolve_session_ref(&*rig.store, prefix).await.is_err());
    rig.quit().await;
}
