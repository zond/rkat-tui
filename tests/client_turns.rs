//! The client's turns end to end, headless: effects in, UI messages out, through the
//! `rkat-tui-mob` daemon with a scripted LLM and an attached client. Covers
//! a turn, a queued turn, a steered message applied at a tool boundary
//! (and one typed while a slow shell call runs), interrupt (also during a
//! slow tool), `-c` and `--resume` across daemon lifetimes, and a moved
//! workspace offered its old state (the re-home is `tests/rehome.rs`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/rig.rs"]
mod rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, copy_tree, task_id, user_texts};
use meerkat_core::{AgentEvent, Message};
use rig::{Attached, ScriptedClassifier, Setup};
use rkat_tui::approval::classifier::{ActionClassifier, ClassifierVerdict};
use rkat_tui::bootstrap::realm_home;
use rkat_tui::ui::delivery::PendingId;
use rkat_tui::ui::{Block, Effect, EscalationAnswer, Msg, StatusUpdate, TurnFailure};
use serde_json::json;
use tokio::sync::Notify;

/// A daemon setup whose client starts with `args` (`-c`, `--resume`).
fn client_args(args: &[&str]) -> Setup {
    Setup {
        client_args: args.iter().map(ToString::to_string).collect(),
        ..Setup::default()
    }
}

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
    let mut rig = Attached::start(&ws.root, &llm).await;
    // The workspace's realm lives outside it, under the config directory
    // (the rig's user config root is the workspace, so the engine's
    // `.rkat/session_comms_identity` lands there; no realm does).
    let realm = common::realm_of(&ws.root);
    assert!(realm.starts_with(common::config_dir_for(&ws.root)));
    assert!(
        realm
            .join(rkat_tui::bootstrap::realm_home::MANIFEST_FILE)
            .is_file()
    );
    assert!(!ws.root.join(".rkat").join("tui").exists());

    rig.send(Effect::StartTurn("hi".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "hello there");

    let session = rig.messages().await;
    assert_eq!(user_texts(&session), vec!["hi".to_string()]);
    // The persona reached the model.
    let first = &llm.requests()[0];
    assert!(first.iter().any(|m| matches!(
        m,
        Message::System(system) if system.content.contains("interactive coding agent")
    )));
    assert!(rig.quit().await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_at_mention_attaches_the_file_the_session_receives() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("notes.txt"), "the build uses make\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("read it")]);
    std::fs::create_dir_all(ws.root.join(".rkat")).unwrap();
    std::fs::write(ws.root.join(".rkat/config.toml"), "").unwrap();
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn(
        "what does @notes.txt say? (not @.rkat/config.toml or @nothing.txt)".into(),
    ));
    let _ = rig.turn_finished().await;
    // The UI hears what was attached as it was sent, not what it guessed.
    assert!(rig.seen.iter().any(|msg| matches!(
        msg,
        Msg::Mentions { attached, sealed }
            if attached == &["notes.txt".to_string()]
                && sealed == &[".rkat/config.toml".to_string()]
    )));
    let expected = vec![
        "what does @notes.txt say? (not @.rkat/config.toml or @nothing.txt)\n\n<file path=\"notes.txt\">\nthe build uses make\n</file>"
            .to_string(),
    ];
    assert_eq!(user_texts(&llm.requests()[0]), expected);
    let session = rig.messages().await;
    assert_eq!(user_texts(&session), expected);
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_queued_behind_a_running_turn_runs_next() {
    let ws = Workspace::new();
    let gate = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&gate), Box::new(Step::Text("first answer"))),
        Step::Text("second answer"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("one".into()));
    llm.wait_call(0).await;
    // A second prompt while the first runs: a `Queue` input behind it.
    rig.send(Effect::StartTurn("two".into()));
    gate.notify_one();

    // The second prompt's send races the gate's release: it joins the
    // running turn or runs right after it; either way it is answered next.
    let mut outcome = rig.turn_finished().await;
    if outcome.as_ref().is_ok_and(|s| s.text == "first answer") {
        outcome = rig.turn_finished().await;
    }
    assert_eq!(outcome.expect("turn succeeds").text, "second answer");
    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        user_texts(&requests[1]),
        vec!["one".to_string(), "two".to_string()]
    );
    let session = rig.messages().await;
    assert_eq!(
        user_texts(&session),
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
    let mut rig = Attached::start(&ws.root, &llm).await;
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
    assert_eq!(outcome.expect("turn succeeds").text, "done");

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
        rig.events()
            .iter()
            .any(|event| matches!(event, AgentEvent::BoundaryAppendApplied { .. }))
    );
    let session = rig.messages().await;
    assert_eq!(
        user_texts(&session),
        vec![
            "read the notes".to_string(),
            "also mention the file name".to_string()
        ]
    );
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continue_and_resume_by_id_across_lifetimes() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("remembered")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let first = rig.current_session().await;
    rig.send(Effect::StartTurn("remember me".into()));
    let _ = rig.turn_finished().await;
    rig.quit().await;

    // `-c` with a restarted daemon: the conversation it restored, with its
    // transcript.
    let llm = ScriptedLlmClient::new(vec![Step::Text("continued")]);
    let mut rig = Attached::start_with(&ws.root, &llm, client_args(&["-c"])).await;
    assert_eq!(rig.current_session().await, first);
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
    let _ = rig.turn_finished().await;
    assert_eq!(
        user_texts(&llm.requests()[0]),
        vec!["remember me".to_string(), "still there?".to_string()]
    );
    rig.quit().await;

    // `--resume <id prefix>` of the current conversation: attached to the
    // agent running it, with no switch.
    let prefix = first.to_string()[..30].to_string();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&ws.root, &llm, client_args(&["--resume", &prefix])).await;
    let shown = first.to_string();
    rig.saw(|m| matches!(m, Msg::Status(rkat_tui::ui::StatusUpdate::Session(id)) if *id == shown))
        .await;
    assert!(
        !rig.notices().contains("resumed conversation"),
        "{}",
        rig.notices()
    );
    assert_eq!(rig.current_session().await, first);
    rig.quit().await;
}

/// `--resume` without a reference: the picker offers the earlier
/// conversation first thing; choosing it resumes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_without_a_reference_opens_the_picker_at_start() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let first = rig.current_session().await;
    rig.send(Effect::StartTurn("pick me later".into()));
    let _ = rig.turn_finished().await;
    // Another conversation is the current one when the picker opens.
    rig.send(Effect::NewSession);
    rig.wait_for(|m| matches!(m, Msg::ResetTranscript(blocks) if blocks.is_empty()))
        .await;
    rig.quit().await;

    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&ws.root, &llm, client_args(&["--resume"])).await;
    assert_ne!(rig.current_session().await, first);
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

/// Move the workspace `ws` (its daemon stopped) to the sibling `name`:
/// copied, then removed.
async fn move_workspace(ws: &Workspace, name: &str) -> std::path::PathBuf {
    let moved = ws.sibling(name);
    copy_tree(&ws.root, &moved);
    // A user moves the directory after rkat-tui exited; here the process
    // lives on, and a file operation the service's shutdown left in flight
    // can still land for a moment, so the removal gets three tries. A
    // leftover that persists is reported by name.
    let mut tries = 0;
    loop {
        match std::fs::remove_dir_all(&ws.root) {
            Ok(()) => break,
            Err(err) if tries < 3 => {
                tries += 1;
                eprintln!(
                    "remove_dir_all try {tries}: {err}; present: {:?}",
                    remaining_entries(&ws.root)
                );
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(err) => panic!(
                "remove_dir_all: {err}; left behind: {:#?}",
                remaining_entries(&ws.root)
            ),
        }
    }
    moved
}

/// A conversation in `ws`, then the daemon stopped: its session.
async fn one_conversation(ws: &Workspace, prompt: &str) -> meerkat_core::SessionId {
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let first = rig.current_session().await;
    rig.send(Effect::StartTurn(prompt.into()));
    let _ = rig.turn_finished().await;
    rig.quit().await;
    first
}

/// The workspaces' realms of the rig's config directory beside `root`.
fn realms(root: &std::path::Path) -> std::path::PathBuf {
    common::config_dir_for(root).join(realm_home::WORKSPACES_DIR)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_moved_workspace_starts_anew_and_is_offered_its_old_state() {
    let ws = Workspace::new();
    let first = one_conversation(&ws, "before the move").await;
    let moved = move_workspace(&ws, "renamed-project").await;
    // The realm stays behind, unclaimed: the manifest names the old path,
    // and nothing in the moved workspace says otherwise. A start there
    // gets a new realm and is offered the old one; nothing is adopted.
    let old = realm_home::find(&realms(&moved), &ws.root)
        .unwrap()
        .unwrap();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&moved, &llm, client_args(&["-c"])).await;
    assert_ne!(rig.current_session().await, first);
    rig.wait_for(|msg| {
        matches!(msg, Msg::Notice { text, .. } if text.contains(&old.id) && text.contains("/rehome"))
    })
    .await;
    rig.quit().await;
    assert_eq!(
        realm_home::find(&realms(&moved), &ws.root).unwrap(),
        Some(old)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_without_a_running_turn_is_refused() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start(&ws.root, &llm).await;
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
                result: Err(rkat_tui::ui::delivery::SteerFailure::NotRunning)
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
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("try them".into()));
    let _ = rig.turn_finished().await;
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
    let id = rig.current_session().await;
    rig.quit().await;
    let meta = rig::stored_session(&ws.root, &id)
        .await
        .session_metadata()
        .unwrap();
    assert_eq!(meta.model, common::MODEL);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_prompt_after_a_failed_turn_is_not_added_twice() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::RejectTools, Step::Text("fixed")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("fix the build".into()));
    let outcome = rig.turn_finished().await;
    assert!(
        matches!(outcome, Err(TurnFailure::Failed(_))),
        "{outcome:?}"
    );
    // The failed run committed the prompt.
    let session = rig.messages().await;
    assert_eq!(user_texts(&session), vec!["fix the build".to_string()]);

    // Re-typed (with stray whitespace): a retry, not a second copy.
    rig.send(Effect::StartTurn("  fix the build\n".into()));
    rig.wait_for(
        |msg| matches!(msg, Msg::Notice { text, .. } if text.contains("already in the transcript")),
    )
    .await;
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("retry succeeds").text, "fixed");
    let expected = vec![
        "fix the build".to_string(),
        "(retrying the request above)".to_string(),
    ];
    assert_eq!(user_texts(&llm.requests()[1]), expected);
    let session = rig.messages().await;
    assert_eq!(user_texts(&session), expected);
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
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("fix the build".into()));
    assert!(rig.turn_finished().await.is_err());

    rig.send(Effect::StartTurn("run the tests instead".into()));
    assert_eq!(rig.turn_finished().await.expect("turn").text, "first");
    // Only right after the failure: a repeat after a success is a message.
    rig.send(Effect::StartTurn("run the tests instead".into()));
    assert_eq!(rig.turn_finished().await.expect("turn").text, "second");
    let session = rig.messages().await;
    assert_eq!(
        user_texts(&session),
        vec![
            "fix the build".to_string(),
            "run the tests instead".to_string(),
            "run the tests instead".to_string(),
        ]
    );
    rig.quit().await;
}

/// Allows every reviewed call (the default classifier of the rig): the
/// slow-tool cases are about delivery, not the approval gate.
fn allow_shell() -> Setup {
    Setup::default()
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
    let mut rig = Attached::start_with(&ws.root, &llm, allow_shell()).await;
    rig.send(Effect::StartTurn("do something for a while".into()));
    tool_started(&mut rig, "slow").await;
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
    assert_eq!(outcome.expect("turn succeeds").text, "finished");
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
    let session = rig.messages().await;
    assert!(
        !session
            .iter()
            .any(|m| matches!(m, Message::SystemNotice(_))),
        "nothing was closed by recovery"
    );
    rig.quit().await;
}

/// ↑ takes back a message typed during a slow tool while the runtime still
/// holds it queued (`rkat/recall`, by the input the send admitted): it never
/// reaches the model, and the turn runs on: the tool ends, its result
/// reaches the model, the turn completes.
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
    let mut rig = Attached::start_with(&ws.root, &llm, allow_shell()).await;
    rig.send(Effect::StartTurn("do something for a while".into()));
    tool_started(&mut rig, "slow").await;
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
    assert_eq!(outcome.expect("turn succeeds").text, "finished");
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
    let session = rig.messages().await;
    assert!(!user_texts(&session).contains(&"never mind".to_string()));
    assert!(
        !session
            .iter()
            .any(|m| matches!(m, Message::SystemNotice(_))),
        "nothing was closed by recovery"
    );
    rig.quit().await;
}

/// A recall after the steer joined the run finds nothing to pull back: the
/// daemon reports it reached, cancels nothing, and the turn runs on with
/// the steer delivered.
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
    let mut rig = Attached::start_with(&ws.root, &llm, allow_shell()).await;
    rig.send(Effect::StartTurn("do something for a while".into()));
    tool_started(&mut rig, "slow").await;
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
    assert_eq!(outcome.expect("turn succeeds").text, "finished");
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "no follow-up turn");
    assert!(user_texts(&requests[1]).contains(&"typed while it sleeps".to_string()));
    rig.quit().await;
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
    let classifier = ScriptedClassifier::allowing(vec![ClassifierVerdict::allow(
        "removes an in-project scratch file\nthe user asked to clean up",
    )]);
    let options = Setup::default().classifier(classifier as Arc<dyn ActionClassifier>);
    let mut rig = Attached::start_with(&ws.root, &llm, options).await;
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
    assert_eq!(rig.turn_finished().await.expect("turn").text, "cleaned up");
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
    let mut rig = Attached::start_with(&ws.root, &llm, allow_shell()).await;
    rig.send(Effect::StartTurn("add a test env file".into()));
    let Msg::Escalation(prompt, id) = rig.wait_for(|msg| matches!(msg, Msg::Escalation(..))).await
    else {
        unreachable!()
    };
    assert_eq!(prompt.tool, "write_file");
    assert!(
        prompt
            .policy_reason
            .as_deref()
            .is_some_and(|r| r.contains("protected path: `.env.test` (secrets)")),
        "{prompt:?}"
    );
    rig.send(Effect::RespondEscalation(id, EscalationAnswer::AllowOnce));
    assert_eq!(rig.turn_finished().await.expect("turn").text, "configured");
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
async fn tool_started(rig: &mut Attached, id: &str) {
    rig.saw(|msg| {
        matches!(msg, Msg::Agent(env)
            if matches!(&env.payload, AgentEvent::ToolExecutionStarted { id: started, .. } if started == id))
    })
    .await;
}

/// Feed the runner's messages to `app` (forwarding the effects it answers
/// with) until one matches `until`; returns that message.
async fn pump(
    rig: &mut Attached,
    app: &mut rkat_tui::ui::App,
    until: impl Fn(&Msg) -> bool,
) -> Msg {
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

/// `/context`: the runner adds the session's system prompt, its messages,
/// the instruction files and the TUI's own tools to the UI's header.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_report_describes_the_session() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("AGENTS.md"), "# AGENTS.md\n\n- use tabs\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("hello")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("hi".into()));
    let _ = rig.turn_finished().await;
    // The daemon reads the tool definitions once the agent is idle.
    let session = rig.current_session().await;
    common::within(async {
        while rig.daemon.origins.sizes(&session).is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
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
    assert!(
        text.contains("tool definitions (every model request carries them): "),
        "{text}"
    );
    for family in [
        "schedule: 8 tools",
        "mob: ",
        "comms: ",
        "files: ",
        "notes: ",
    ] {
        assert!(text.contains(family), "{family}: {text}");
    }
    rig.quit().await;
}

/// `/status`: one notice naming the version, model, session and the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_report_names_the_session_and_the_workspace() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StatusReport);
    let msg = rig
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.starts_with("rkat-tui ")))
        .await;
    let Msg::Notice { text, .. } = msg else {
        unreachable!()
    };
    assert!(
        text.contains(&format!("session: {}", rig.current_session().await)),
        "{text}"
    );
    assert!(
        text.contains(&format!("workspace: {}", ws.root.display())),
        "{text}"
    );
    assert!(text.contains("attached to the workspace daemon"), "{text}");
    assert!(text.contains("approval gate: in the daemon"), "{text}");
    // The daemon's side (`rkat/report`): the realm, the shell's
    // confinement, the permission mode, the realm rules file and the
    // helpers' depth limit.
    let realm = rkat_tui::bootstrap::resolve_scope(&common::args_for(&ws.root))
        .unwrap()
        .realm_root();
    assert!(
        text.contains(&format!("realm: {}", realm.display())),
        "{text}"
    );
    assert!(text.contains("shell confinement: off"), "{text}");
    assert!(text.contains("permission mode: auto"), "{text}");
    assert!(
        text.contains(&format!(
            "realm rules: {}",
            realm.join("tui.toml").display()
        )),
        "{text}"
    );
    assert!(
        text.contains("helpers: depth limit on (max_depth 1"),
        "{text}"
    );
    rig.quit().await;
}

/// `/export`: the runner writes the UI's text to the default file in the
/// workspace's realm (and to a typed path), and says where.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_writes_the_transcript_and_names_the_file() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let id = rig.current_session().await;
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
    assert!(path.starts_with(common::realm_of(&ws.root)), "{text}");
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with(&format!("export-{id}-")) && name.ends_with(".txt"),
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

/// The runner's quit releases the lease the bootstrap took.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quitting_releases_the_realm_lease() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let rig = Attached::start(&ws.root, &llm).await;
    let realm = common::realm_of(&ws.root);
    assert_eq!(walk_leases(&realm), 1);
    rig.quit().await;
    assert_eq!(walk_leases(&realm), 0);
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

/// `!echo hi` through the runner: the command runs in the workspace (here
/// unconfined, like the agent's shell under `--no-confinement`), its output
/// comes back, and the next model call reads it as system context without a
/// turn having been started for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bang_command_runs_and_joins_the_context_without_a_turn() {
    use rkat_tui::ui::{ShellOutcome, ShellRunId};
    use rkat_tui::user_shell::ShellExit;

    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("seen")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::RunShell {
        id: ShellRunId(1),
        command: "echo hi; pwd".into(),
    });
    let msg = rig
        .wait_for(|msg| matches!(msg, Msg::ShellFinished { .. }))
        .await;
    let Msg::ShellFinished {
        id,
        outcome: ShellOutcome::Ran { run, context },
    } = msg
    else {
        panic!("{msg:?}");
    };
    assert_eq!(id, ShellRunId(1));
    assert_eq!(run.exit, ShellExit::Code(0));
    let root = std::fs::canonicalize(&ws.root).unwrap();
    assert!(run.output.contains("hi\n"), "{run:?}");
    assert!(run.output.contains(&root.display().to_string()), "{run:?}");
    assert_eq!(context, Ok(()));
    // No model call yet: the command started no turn.
    assert!(llm.recorded().is_empty());

    rig.send(Effect::StartTurn("what did it say?".into()));
    let _ = rig.turn_finished().await;
    let system = llm.recorded()[0].system();
    assert!(
        system.contains("[shell run by the user] $ echo hi; pwd\nhi\n"),
        "{system}"
    );
    assert!(system.contains("(exit 0)"), "{system}");
    assert_eq!(
        user_texts(&llm.requests()[0]),
        vec!["what did it say?".to_string()]
    );
    rig.quit().await;
}

/// Plan mode end to end: the session is told, a write the model tries is
/// refused with the plan-mode error the model reads (nothing is written,
/// nobody is asked), the mode is recorded in the realm's `tui.toml`, and
/// leaving plan mode tells the session so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_mode_refuses_a_write_and_the_model_reads_why() {
    use rkat_tui::approval::mode::PermissionMode;

    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "w",
            name: "write_file",
            args: json!({ "path": "src/new.rs", "content": "pub fn f() {}\n" }),
        },
        Step::Text("1. Add src/new.rs with f()"),
        Step::Text("ok"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let mode_is = |want: PermissionMode| move |msg: &Msg| matches!(msg, Msg::Status(StatusUpdate::Mode { mode, .. }) if *mode == want);
    rig.send(Effect::SetMode(PermissionMode::Plan));
    rig.wait_for(mode_is(PermissionMode::Plan)).await;
    rig.send(Effect::StartTurn("plan a new module".into()));
    assert_eq!(
        rig.turn_finished().await.expect("turn").text,
        "1. Add src/new.rs with f()"
    );
    assert!(!ws.root.join("src/new.rs").exists());
    assert!(
        !rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::Escalation(..))),
        "nobody is asked"
    );
    let result = llm
        .requests()
        .iter()
        .flatten()
        .find_map(|m| match m {
            Message::ToolResults { results, .. } => {
                results.iter().find(|r| r.tool_use_id == "w").cloned()
            }
            _ => None,
        })
        .expect("a result for the write");
    assert!(result.is_error, "{result:?}");
    assert!(
        result.text_content().contains("plan mode is on"),
        "{}",
        result.text_content()
    );
    let system = llm.recorded()[0].system();
    assert!(system.contains("# Plan mode is on"), "{system}");
    let realm_rules = std::fs::read_to_string(common::realm_of(&ws.root).join("tui.toml")).unwrap();
    assert!(realm_rules.contains("mode = \"plan\""), "{realm_rules}");

    rig.send(Effect::SetMode(PermissionMode::AcceptEdits));
    rig.wait_for(mode_is(PermissionMode::AcceptEdits)).await;
    rig.send(Effect::StartTurn("go".into()));
    let _ = rig.turn_finished().await;
    let system = llm.recorded().last().unwrap().system();
    assert!(system.contains("# Plan mode is off"), "{system}");
    let realm_rules = std::fs::read_to_string(common::realm_of(&ws.root).join("tui.toml")).unwrap();
    assert!(
        realm_rules.contains("mode = \"accept-edits\""),
        "{realm_rules}"
    );
    rig.quit().await;
}

/// The model's task tools fill the Ctrl+T checklist from the engine's task
/// store, and a resumed session gets its checklist back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_task_tools_fill_the_checklist_and_resume_restores_it() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use rkat_tui::ui::{App, ChecklistView, ItemState, StatusInfo};

    let ws = Workspace::new();
    let create = |subject: &str| json!({ "subject": subject, "description": "" });
    let llm = ScriptedLlmClient::new(vec![
        Step::Tools(vec![
            ("c1", "task_create", create("read the parser")),
            ("c2", "task_create", create("write the fix")),
            ("c3", "task_create", create("run the tests")),
        ]),
        Step::Dynamic(Arc::new(|messages| {
            Step::Tools(vec![
                (
                    "u1",
                    "task_update",
                    json!({ "id": task_id(messages, "read the parser"), "status": "completed" }),
                ),
                (
                    "u2",
                    "task_update",
                    json!({ "id": task_id(messages, "write the fix"), "status": "in_progress" }),
                ),
            ])
        })),
        Step::Text("on it"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let first = rig.current_session().await;
    let mut app = App::new(StatusInfo::default());
    rig.send(Effect::StartTurn("fix the parser".into()));
    let done_and_in_progress = |msg: &Msg| {
        matches!(msg, Msg::Checklist(list) if list.items().iter().any(|i| i.state == ItemState::InProgress)
            && list.items().iter().any(|i| i.state == ItemState::Done))
    };
    pump(&mut rig, &mut app, done_and_in_progress).await;
    let _ = rig.turn_finished().await;
    let ctrl_t = Msg::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    let fx = app.update(ctrl_t.clone());
    assert!(fx.contains(&Effect::SetChecklistView(ChecklistView::Shown)));
    for effect in fx {
        rig.send(effect);
    }
    let want = [
        ("run the tests", ItemState::Pending),
        ("read the parser", ItemState::Done),
        ("write the fix", ItemState::InProgress),
    ];
    // The two updates ran as parallel tool calls, so their order among
    // themselves is not fixed: the pending item first, the others as a set.
    let items = |list: &rkat_tui::ui::Checklist| -> Vec<(String, ItemState)> {
        let mut items: Vec<(String, ItemState)> = list
            .items()
            .iter()
            .map(|i| (i.text.clone(), i.state))
            .collect();
        if items.len() > 1 {
            items[1..].sort_by(|a, b| a.0.cmp(&b.0));
        }
        items
    };
    let want: Vec<(String, ItemState)> = want.iter().map(|(t, s)| (t.to_string(), *s)).collect();
    assert_eq!(items(app.checklist_panel().expect("shown")), want);
    // The creates showed up before the updates (a read after each
    // completed task call).
    assert!(
        rig.seen.iter().any(|m| matches!(m, Msg::Checklist(list)
            if list.items().len() == 3 && list.items().iter().all(|i| i.state == ItemState::Pending))),
        "{:?}",
        rig.seen
    );
    rig.quit().await;
    let realm_rules = std::fs::read_to_string(common::realm_of(&ws.root).join("tui.toml")).unwrap();
    assert!(
        realm_rules.contains("checklist = \"shown\""),
        "{realm_rules}"
    );

    // Restored by a restarted daemon: the same list, read from the store,
    // before any turn.
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&ws.root, &llm, client_args(&["-c"])).await;
    assert_eq!(rig.current_session().await, first);
    rig.saw(|msg| matches!(msg, Msg::Checklist(list) if !list.is_empty()))
        .await;
    let list = rig
        .seen
        .iter()
        .find_map(|msg| match msg {
            Msg::Checklist(list) if !list.is_empty() => Some(list.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(items(&list), want);

    // A new conversation starts with an empty checklist.
    rig.send(Effect::NewSession);
    rig.wait_for(|msg| matches!(msg, Msg::ResetTranscript(blocks) if blocks.is_empty()))
        .await;
    let msg = rig.wait_for(|msg| matches!(msg, Msg::Checklist(_))).await;
    assert!(matches!(msg, Msg::Checklist(list) if list.is_empty()));
    rig.quit().await;
}

/// Every path under `root`, for the rename test's diagnostics.
fn remaining_entries(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            out.push(path.display().to_string());
            if path.is_dir() {
                out.extend(remaining_entries(&path));
            }
        }
    }
    out
}
