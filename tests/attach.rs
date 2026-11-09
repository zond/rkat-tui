//! Attach mode: the TUI's attach runner (`rkat-tui --attach`) against the
//! `rkat-tui-mob` daemon started in-process with a scripted LLM, driven
//! through the same `Effect`/`Msg` channels the UI uses.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, args_for, endpoint_file_for, no_lists, within};
use meerkat_core::AgentEvent;
use rig::{Client, Running};
use rkat_tui::cli::AttachMode;
use rkat_tui::client::catchup::CursorFile;
use rkat_tui::client::runner::RECALL_UNAVAILABLE;
use rkat_tui::client::sessions::REWIND_UNAVAILABLE;
use rkat_tui::client::testing::spawn_attach_runner;
use rkat_tui::ui::{Effect, Msg, StatusInfo, StatusUpdate, TurnFailure};
use rkat_tui::workbench::build_app;

/// The command line's prompt is sent as the UI opens, before the first
/// attach finished: it is held until the link is up, not failed as "not
/// attached" (with attach mode the default, `rkat-tui "fix it"` relies
/// on it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_sent_before_the_attach_finished_is_held_until_it_did() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("answered")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut config = rkat_tui::client::configure(&args_for(&ws.root), AttachMode::Existing)
        .await
        .unwrap();
    config.lister = no_lists();
    config.endpoint_files = vec![endpoint_file_for(&ws.root)];
    let mut client = Client {
        harness: spawn_attach_runner(config),
        seen: Vec::new(),
    };
    client.send(Effect::StartTurn("from the command line".into()));
    let outcome = client.turn_finished().await;
    assert_eq!(outcome.result.expect("completed").text, "answered");
    assert!(llm.recorded()[0].user().contains("from the command line"));
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_is_answered_and_rendered() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("hello from the daemon")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    assert!(
        client
            .notices()
            .contains(rkat_tui::client::runner::ATTACH_NOTICE)
    );
    client.send(Effect::StartTurn("say hello".into()));
    let outcome = client.turn_finished().await;
    let summary = outcome.result.expect("completed");
    assert_eq!(summary.text, "hello from the daemon");
    assert_eq!(client.texts("hello from the daemon"), 1);
    assert!(llm.recorded()[0].user().contains("say hello"));
    // A turn this client started is not announced as a remote one.
    assert!(
        !client
            .seen
            .iter()
            .any(|m| matches!(m, Msg::RemoteTurn { .. }))
    );
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn esc_interrupts_a_hanging_turn() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Hang, Step::Text("after")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("hang".into()));
    client
        .wait_for(|msg| matches!(msg, Msg::Agent(env) if matches!(env.payload, AgentEvent::RunStarted { .. })))
        .await;
    llm.wait_call(0).await;
    client.send(Effect::Interrupt);
    let outcome = client.turn_finished().await;
    assert_eq!(
        outcome.result,
        Err(TurnFailure::Interrupted),
        "{:#?}",
        client.seen
    );
    // The member serves the next prompt.
    client.send(Effect::StartTurn("again".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "after");
    client.quit().await;
    daemon.stop().await;
}

/// `/model` in attach mode switches the session in place
/// (`rkat/set_model`, as the in-process switch): the session keeps the
/// model, the next prompt is an ordinary send, and the switch is tied to no
/// connection (the client that made it can leave; another client's prompt
/// runs on the new model).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_switches_in_place_and_stays() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Text("one"),
        Step::Text("two"),
        Step::Text("three"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("first".into()));
    client.turn_finished().await.result.unwrap();
    client.send(Effect::SetModel("anthropic:claude-opus-4-8".into()));
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.starts_with("switched to claude-opus-4-8")))
        .await;
    assert!(
        client.seen.iter().any(|msg| matches!(msg, Msg::Status(StatusUpdate::Model { model, .. }) if model == "claude-opus-4-8"))
    );
    // A model this mode cannot name is refused before anything is sent.
    client.send(Effect::SetModel("my-alias".into()));
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("cannot tell which endpoint serves `my-alias`")))
        .await;
    client.quit().await;

    // Another client, after the first left: the session runs the new model.
    let mut other = Client::attach(&ws.root).await;
    other
        .saw(|msg| matches!(msg, Msg::Status(StatusUpdate::Model { model, .. }) if model == "claude-opus-4-8"))
        .await;
    other.send(Effect::StartTurn("second".into()));
    assert_eq!(other.turn_finished().await.result.unwrap().text, "two");
    other.send(Effect::StartTurn("third".into()));
    assert_eq!(other.turn_finished().await.result.unwrap().text, "three");
    let models: Vec<String> = llm.recorded().into_iter().map(|r| r.model).collect();
    assert_eq!(
        models,
        vec!["claude-sonnet-4-6", "claude-opus-4-8", "claude-opus-4-8"]
    );
    other.quit().await;
    daemon.stop().await;
}

/// A model switch is refused while a turn runs, as in-process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_model_switch_waits_for_the_turn() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Hang]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("hang".into()));
    llm.wait_call(0).await;
    client.send(Effect::SetModel("anthropic:claude-opus-4-8".into()));
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("a turn is running; switch models once it ends")))
        .await;
    client.send(Effect::Interrupt);
    client.turn_finished().await;
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_left_mid_turn_sees_the_turn_once_on_reattach() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Text("before"),
        Step::Gated(
            Arc::clone(&gate),
            Box::new(Step::Text("finished without you")),
        ),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("warm up".into()));
    client.turn_finished().await.result.unwrap();
    client.send(Effect::StartTurn("long work".into()));
    client
        .wait_for(|msg| matches!(msg, Msg::Agent(env) if matches!(&env.payload, AgentEvent::RunStarted { input, .. } if input.prompt_text().as_deref() == Some("long work"))))
        .await;
    llm.wait_call(1).await;
    // Detach mid-turn; the turn goes on in the daemon.
    client.quit().await;
    gate.notify_one();
    let cursor = CursorFile::of_workspace(&ws.root)
        .load()
        .expect("cursor saved");
    // The reattach resumes the stream from this position (same daemon).
    assert!(
        cursor.epoch.is_some() && cursor.daemon.is_some(),
        "{cursor:?}"
    );

    let mut client = Client::attach(&ws.root).await;
    let outcome = client.turn_finished().await;
    assert_eq!(outcome.result.unwrap().text, "finished without you");
    // Only what the first client had not shown is replayed: the earlier
    // turn is not shown again and the completed one exactly once.
    assert_eq!(client.texts("finished without you"), 1);
    assert_eq!(client.texts("before"), 0);
    let shown: Vec<u64> = client
        .seen
        .iter()
        .filter_map(|m| match m {
            Msg::Agent(env) => Some(env.seq),
            _ => None,
        })
        .collect();
    assert!(
        shown.iter().all(|seq| *seq > cursor.sequence),
        "{shown:?} after {cursor:?}"
    );
    let mut sorted = shown.clone();
    sorted.dedup();
    assert_eq!(sorted, shown, "an event was shown twice");
    // The run started before this client attached: it was in progress, so
    // it came as a remote turn (the prompt was already shown before).
    assert!(
        client
            .seen
            .iter()
            .any(|m| matches!(m, Msg::RemoteTurn { .. }))
    );
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_client_reconnects_to_a_restarted_daemon() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("first life"), Step::Text("second life")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("one".into()));
    assert_eq!(
        client.turn_finished().await.result.unwrap().text,
        "first life"
    );
    daemon.stop().await;
    client
        .wait_for(|msg| matches!(msg, Msg::Status(StatusUpdate::Attach(Some(state))) if state == "reconnecting"))
        .await;
    let daemon = Running::start(&ws.root, &llm).await;
    client.attached().await;
    // A new daemon numbers its events in a new space: the client does not
    // resume its cursor there but renders the conversation from its start.
    let rendered = client
        .wait_for(|msg| matches!(msg, Msg::ResetTranscript(_)))
        .await;
    let Msg::ResetTranscript(blocks) = rendered else {
        unreachable!()
    };
    assert!(
        blocks
            .iter()
            .any(|b| matches!(b, rkat_tui::ui::Block::User { text } if text == "one")),
        "{blocks:?}"
    );
    let before = CursorFile::of_workspace(&ws.root).load().unwrap();
    client.send(Effect::StartTurn("two".into()));
    assert_eq!(
        client.turn_finished().await.result.unwrap().text,
        "second life"
    );
    assert_eq!(
        client.texts("first life"),
        1,
        "the catch-up did not repeat the first turn"
    );
    assert!(llm.recorded()[1].user().contains("two"));
    let after = CursorFile::of_workspace(&ws.root).load().unwrap();
    assert_ne!(after.daemon, before.daemon, "the new daemon's cursor");
    assert!(after.epoch.is_some());
    client.quit().await;
    daemon.stop().await;
}

/// Ctrl+T in attach mode: the agent's task tools fill the checklist from
/// the daemon's task store (`rkat/tasks`), the view is saved as the realm's
/// `[ui] checklist` by the daemon, and a client attaching later gets the
/// list before any turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_task_tools_fill_the_checklist_over_the_daemon() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use rkat_tui::ui::{App, ChecklistView, ItemState};

    let ws = Workspace::new();
    let create = |subject: &str| serde_json::json!({ "subject": subject, "description": "" });
    let llm = ScriptedLlmClient::new(vec![
        Step::Tools(vec![
            ("c1", "task_create", create("read the parser")),
            ("c2", "task_create", create("write the fix")),
        ]),
        Step::Dynamic(Arc::new(|messages| Step::Tool {
            id: "u1",
            name: "task_update",
            args: serde_json::json!({
                "id": common::task_id(messages, "read the parser"),
                "status": "completed",
            }),
        })),
        Step::Text("on it"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("fix the parser".into()));
    client.turn_finished().await.result.unwrap();
    let want = vec![
        ("write the fix".to_string(), ItemState::Pending),
        ("read the parser".to_string(), ItemState::Done),
    ];
    let items = |list: &rkat_tui::ui::Checklist| -> Vec<(String, ItemState)> {
        list.items()
            .iter()
            .map(|i| (i.text.clone(), i.state))
            .collect()
    };
    let updated = |msg: &Msg| matches!(msg, Msg::Checklist(list) if list.items().iter().any(|i| i.state == ItemState::Done));
    client.saw(updated).await;
    let Some(Msg::Checklist(list)) = client.seen.iter().find(|msg| updated(msg)).cloned() else {
        unreachable!()
    };
    assert_eq!(items(&list), want);
    // The creates showed before the update.
    assert!(
        client.seen.iter().any(|m| matches!(m, Msg::Checklist(list)
            if list.items().len() == 2 && list.items().iter().all(|i| i.state == ItemState::Pending))),
        "{:?}",
        client.seen
    );
    // Ctrl+T, as the UI turns it into an effect.
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::Checklist(list));
    let fx = app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
    )));
    assert!(fx.contains(&Effect::SetChecklistView(ChecklistView::Shown)));
    for effect in fx {
        client.send(effect);
    }
    let realm = ws.root.join(".rkat/tui/tui.toml");
    within(async {
        loop {
            let text = std::fs::read_to_string(&realm).unwrap_or_default();
            if text.contains("checklist = \"shown\"") {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        !client.notices().contains("was not saved"),
        "{}",
        client.notices()
    );
    client.quit().await;

    // Another client, later: the list before any turn.
    let mut later = Client::attach(&ws.root).await;
    let msg = later
        .wait_for(|msg| matches!(msg, Msg::Checklist(list) if !list.is_empty()))
        .await;
    let Msg::Checklist(list) = msg else {
        unreachable!()
    };
    assert_eq!(items(&list), want);
    later.quit().await;
    daemon.stop().await;
}

/// `/context` in attach mode: the conversation's messages read from the
/// daemon, the instruction files on disk, as in-process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_reports_the_daemons_conversation() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("AGENTS.md"), "be brief\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("hi")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("hello".into()));
    client.turn_finished().await.result.unwrap();
    client.send(Effect::ContextReport("context: the gauge".into()));
    let msg = client
        .wait_for(
            |msg| matches!(msg, Msg::Notice { text, .. } if text.starts_with("context: the gauge")),
        )
        .await;
    let Msg::Notice { text, .. } = msg else {
        unreachable!()
    };
    assert!(text.contains("system prompt: "), "{text}");
    assert!(text.contains("messages: 2 (user 1, assistant 1"), "{text}");
    assert!(text.contains("workspace AGENTS.md: "), "{text}");
    assert!(text.contains("9 bytes"), "{text}");
    assert!(text.contains("in the daemon"), "{text}");
    client.quit().await;
    daemon.stop().await;
}

/// The agent's file changes show as diffs in every attached client, as
/// in-process (the daemon pushes `rkat/file_diff`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agents_file_changes_show_as_diffs() {
    use rkat_tui::approval::mode::PermissionMode;

    let ws = Workspace::new();
    std::fs::write(ws.root.join("notes.txt"), "old line\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "r1",
            name: "read_file",
            args: serde_json::json!({"path": "notes.txt"}),
        },
        Step::Tool {
            id: "w1",
            name: "write_file",
            args: serde_json::json!({"path": "notes.txt", "content": "new line\n"}),
        },
        Step::Text("done"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    let mut other = Client::attach(&ws.root).await;
    client.set_mode(PermissionMode::AcceptEdits).await;
    client.send(Effect::StartTurn("edit the notes".into()));
    client.turn_finished().await.result.unwrap();
    for seen in [&mut client, &mut other] {
        let is_diff = |msg: &Msg| matches!(msg, Msg::Diff { .. });
        seen.saw(is_diff).await;
        let Some(Msg::Diff {
            tool_use_id,
            path,
            unified,
            added,
            removed,
        }) = seen.seen.iter().find(|msg| is_diff(msg)).cloned()
        else {
            unreachable!()
        };
        assert_eq!(tool_use_id, "w1");
        assert_eq!(path, "notes.txt");
        assert_eq!((added, removed), (1, 1));
        assert!(unified.contains("+new line"), "{unified}");
    }
    client.quit().await;
    other.quit().await;
    daemon.stop().await;
}

/// The daemon's start-up notices (here: its shell runs unconfined) show in
/// a client when it first attaches, as the in-process TUI shows its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemons_start_up_notices_show_on_attach() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client
        .saw(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("confinement: off")))
        .await;
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_the_daemon_cannot_do_yet_says_so() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::RecallQueued);
    client
        .wait_for(|msg| matches!(msg, Msg::Recalled { errors, .. } if errors.iter().any(|e| e == RECALL_UNAVAILABLE)))
        .await;
    client.send(Effect::RequestRewindMenu);
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text == REWIND_UNAVAILABLE))
        .await;
    client.send(Effect::StatusReport);
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("attached to the workspace daemon") && text.contains("orchestrator `main`")))
        .await;
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bang_command_runs_in_this_process_and_joins_the_agents_context() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("seen it")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::RunShell {
        id: rkat_tui::ui::ShellRunId(1),
        command: "echo bang-$((40+2))".into(),
    });
    let finished = client
        .wait_for(|msg| matches!(msg, Msg::ShellFinished { .. }))
        .await;
    let Msg::ShellFinished {
        outcome: rkat_tui::ui::ShellOutcome::Ran { run, context },
        ..
    } = finished
    else {
        panic!("{finished:?}");
    };
    assert!(run.output.contains("bang-42"), "{}", run.output);
    context.expect("the output joined the orchestrator's context");
    client.send(Effect::StartTurn("what did I run?".into()));
    client.turn_finished().await.result.unwrap();
    let request = format!("{:?}", llm.requests()[0]);
    assert!(request.contains("bang-42"), "the agent read the output");
    client.quit().await;
    daemon.stop().await;
}

#[test]
fn attach_mode_is_the_default_and_a_users_tui_toml_can_turn_it_off() {
    use rkat_tui::bootstrap::{attach_mode as of_layers, read_layers};
    let attach_mode = |args: &rkat_tui::Args| of_layers(args, &read_layers(args).unwrap());
    let ws = Workspace::new();
    let args = args_for(&ws.root);
    // Neither flag nor tui.toml: attach, starting the daemon when none runs.
    assert_eq!(attach_mode(&args), Some(AttachMode::Auto));
    // A user layer (the --config-dir tui.toml) turns it off, and on.
    let user = ws.root.join(".cfg");
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(user.join("tui.toml"), "attach = false\n").unwrap();
    assert_eq!(attach_mode(&args), None);
    assert_eq!(
        attach_mode(&common::args_with(&ws.root, &["--attach"])),
        Some(AttachMode::Auto)
    );
    std::fs::write(user.join("tui.toml"), "attach = true\n").unwrap();
    assert_eq!(attach_mode(&args), Some(AttachMode::Auto));
    // The flags win.
    assert_eq!(
        attach_mode(&common::args_with(&ws.root, &["--no-attach"])),
        None
    );
    assert_eq!(
        attach_mode(&common::args_with(&ws.root, &["--attach=existing"])),
        Some(AttachMode::Existing)
    );
    // A project file cannot choose it.
    std::fs::remove_file(user.join("tui.toml")).unwrap();
    std::fs::create_dir_all(ws.root.join(".rkat")).unwrap();
    std::fs::write(ws.root.join(".rkat/tui.toml"), "attach = false\n").unwrap();
    assert_eq!(attach_mode(&args), Some(AttachMode::Auto));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_steer_in_turn_and_both_see_the_whole_conversation() {
    use rkat_tui::ui::delivery::PendingId;
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&gate), Box::new(Step::Text("first answer"))),
        Step::Text("second answer"),
        Step::Text("third answer"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = Client::attach(&ws.root).await;
    let mut b = Client::attach(&ws.root).await;
    a.send(Effect::StartTurn("from A".into()));
    llm.wait_call(0).await;
    // B sees A's turn running and steers into it.
    b.wait_for(|m| matches!(m, Msg::RemoteTurn { prompt: Some(p) } if p == "from A"))
        .await;
    b.send(Effect::Steer {
        id: PendingId(1),
        text: "steer from B".into(),
    });
    b.wait_for(|m| {
        matches!(
            m,
            Msg::SteerAck {
                id: PendingId(1),
                result: Ok(())
            }
        )
    })
    .await;
    gate.notify_one();
    llm.wait_call(1).await;
    let steered = common::user_texts(&llm.recorded()[1].messages);
    assert!(
        steered.iter().any(|t| t.contains("steer from B")),
        "{steered:?}"
    );
    // Then B prompts and A sees it (once both showed the steered run end,
    // in the first turn or as a turn of its own).
    let settled = |client: &Client| {
        let shown = client.seen.iter().position(|m| {
            matches!(m, Msg::Agent(env) if matches!(&env.payload, AgentEvent::TextComplete { content, .. } if content == "second answer"))
        });
        shown.is_some_and(|at| {
            client.seen[at..]
                .iter()
                .any(|m| matches!(m, Msg::TurnFinished(_)))
        })
    };
    within(async {
        while !settled(&a) || !settled(&b) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            for client in [&mut a, &mut b] {
                while let Ok(msg) = client.harness.ui_rx.try_recv() {
                    client.seen.push(msg);
                }
            }
        }
    })
    .await;
    b.send(Effect::StartTurn("from B".into()));
    assert_eq!(b.turn_finished().await.result.unwrap().text, "third answer");
    a.wait_for(|m| matches!(m, Msg::RemoteTurn { prompt: Some(p) } if p == "from B"))
        .await;
    a.turn_finished().await.result.unwrap();
    for client in [&a, &b] {
        for text in ["first answer", "second answer", "third answer"] {
            assert_eq!(client.texts(text), 1, "{text}");
        }
    }
    a.quit().await;
    b.quit().await;
    daemon.stop().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reconnect_restarts_a_daemon_that_died() {
    let ws = Workspace::new();
    // The real daemon below needs an endpoint with a key (any variable that
    // is set: it never calls the endpoint).
    let user = ws.root.join(".cfg");
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(
        user.join("tui.toml"),
        "[endpoints.fake]\ndialect = \"anthropic\"\nbase_url = \"http://127.0.0.1:9\"\napi_key_env = \"HOME\"\n",
    )
    .unwrap();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let args = args_for(&ws.root);
    let mut config = rkat_tui::client::configure(&args, AttachMode::Auto)
        .await
        .unwrap();
    config.lister = no_lists();
    config.endpoint_files = vec![endpoint_file_for(&ws.root)];
    if let Some(plan) = &mut config.spawn {
        plan.program = std::path::PathBuf::from(env!("CARGO_BIN_EXE_rkat-tui-mob"));
    }
    let endpoint_file = config.endpoint_files[0].clone();
    let mut client = Client {
        harness: spawn_attach_runner(config),
        seen: Vec::new(),
    };
    client.attached().await;
    // The daemon dies and leaves its endpoint file behind.
    let left = std::fs::read(&endpoint_file).unwrap();
    daemon.stop().await;
    std::fs::write(&endpoint_file, left).unwrap();
    client
        .wait_for(
            |m| matches!(m, Msg::Notice { text, .. } if text.contains("restarted rkat-tui-mob")),
        )
        .await;
    client.attached().await;
    // Stop the restarted one, which removes its endpoint file.
    client.send(Effect::DaemonStop);
    client
        .wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.contains("asked the daemon")))
        .await;
    within(async {
        while endpoint_file.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    client.quit().await;
}

/// The findings this guards: attach mode never pushed prices (`/cost`
/// showed `$?`) and its `!` hint said "sandbox" under `--no-confinement`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_mode_prices_the_cost_readout_and_names_an_unconfined_shell() {
    let ws = Workspace::new();
    // The rig runs unconfined (`--no-confinement`).
    let args = common::args_with(
        &ws.root,
        &[
            "--set",
            "pricing.priced-model.input=3",
            "--set",
            "pricing.priced-model.output=15",
        ],
    );
    let mut config = rkat_tui::client::configure(&args, AttachMode::Existing)
        .await
        .unwrap();
    config.lister = no_lists();
    config.endpoint_files = vec![endpoint_file_for(&ws.root)];
    let app = build_app(
        &rkat_tui::client::app_setup(&config),
        StatusInfo::default(),
        &[],
        &[],
    );
    assert!(app.shell_mode_hint().contains("unconfined"));
    let mut harness = spawn_attach_runner(config);
    let priced = within(async {
        loop {
            if let Some(Msg::Status(StatusUpdate::Prices(table))) = harness.ui_rx.recv().await {
                return table;
            }
        }
    })
    .await;
    assert!(priced.lookup("priced-model").is_some());
    harness.effects.send(Effect::Quit).unwrap();
    within(harness.task).await.unwrap();
}

/// The finding this guards: the two runners built their UI from two copies
/// of the start-up code, which drifted. Both now go through
/// `workbench::build_app`, each reading its `AppSetup` its own way (the
/// runtime in process, the `tui.toml` layers in attach mode); for the same
/// configuration the setups, and so the UIs, are the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_runners_build_the_same_app_for_the_same_config() {
    let ws = Workspace::new();
    let args = common::args_with(
        &ws.root,
        &[
            "--set",
            "ui.bell=false",
            "--set",
            "ui.helpers=\"expanded\"",
            "--set",
            "auto.compact=\"warn\"",
            "--set",
            "auto.compact_warn_percent=60",
            // Below the warning: both clamp it up to 60.
            "--set",
            "auto.compact_auto_percent=50",
        ],
    );
    let llm = ScriptedLlmClient::new(vec![]);
    let mut runtime = rkat_tui::bootstrap::bootstrap(&args, Running::bootstrap(&llm))
        .await
        .unwrap();
    let in_process = rkat_tui::runner::app_setup(&runtime);
    runtime.release_realm_lease().await;
    let config = rkat_tui::client::configure(&args, AttachMode::Existing)
        .await
        .unwrap();
    let attach = rkat_tui::client::app_setup(&config);
    assert_eq!(in_process, attach);
    assert!(!attach.ui.bell);
    assert_eq!(
        (
            attach.compaction.warn_percent,
            attach.compaction.auto_percent
        ),
        (60, 60)
    );
    assert!(!attach.shell_confined, "the rig runs --no-confinement");
    let (a, b) = (
        build_app(&in_process, StatusInfo::default(), &[], &[]),
        build_app(&attach, StatusInfo::default(), &[], &[]),
    );
    assert_eq!(a.compaction_policy(), b.compaction_policy());
    assert_eq!(a.shell_mode_hint(), b.shell_mode_hint());
}

/// The finding this guards: attach mode dropped the messages the UI still
/// held at quit and never said that earlier ones were saved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsent_messages_are_saved_at_quit_and_announced_at_the_next_attach() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let client = Client::attach(&ws.root).await;
    client.send(Effect::SaveUnsent(vec![(
        Some(rkat_tui::ui::delivery::PendingId(1)),
        "for later".into(),
    )]));
    let saved = client.quit().await.expect("saved");
    assert_eq!(saved.count, 1);
    assert!(
        std::fs::read_to_string(&saved.path)
            .unwrap()
            .contains("for later")
    );
    let mut again = Client::attach(&ws.root).await;
    again
        .saw(|msg| matches!(msg, Msg::Notice { text, .. } if text.starts_with("1 unsent message")))
        .await;
    assert!(again.quit().await.is_none());
    daemon.stop().await;
}

/// The finding this guards: the workspace and its `tui.toml` layers were
/// read once to pick attach mode and again by the runner; with
/// `--isolated` the two reads minted two realms. The runner is configured
/// from the layers read at start.
#[tokio::test]
async fn attach_mode_runs_on_the_layers_read_at_start() {
    let ws = Workspace::new();
    let args = common::args_with(&ws.root, &["--isolated"]);
    let layers = rkat_tui::bootstrap::read_layers(&args).unwrap();
    let realm = layers.scope.realm_id.clone();
    let config = rkat_tui::client::configure_with(layers, &args, AttachMode::Existing)
        .await
        .unwrap();
    assert_eq!(config.scope.realm_id, realm);
    assert_ne!(
        rkat_tui::bootstrap::read_layers(&args)
            .unwrap()
            .scope
            .realm_id,
        realm,
        "a second read mints another isolated realm"
    );
}

/// ↑/↓ recall and Ctrl+R search in attach mode: the attach runner's UI
/// loads the same history files as the in-process one (the realm's and the
/// user's), a line submitted in one is appended to them, and the next start
/// of either recalls it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_mode_recalls_and_searches_the_shared_input_history() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use rkat_tui::workbench::app::load_histories;

    let ws = Workspace::new();
    let args = args_for(&ws.root);
    let llm = ScriptedLlmClient::new(vec![]);
    let mut runtime = rkat_tui::bootstrap::bootstrap(&args, Running::bootstrap(&llm))
        .await
        .unwrap();
    let (in_process, in_process_global) = load_histories(&runtime.scope);
    runtime.release_realm_lease().await;
    let config = rkat_tui::client::configure(&args, AttachMode::Existing)
        .await
        .unwrap();
    let (mut history, mut global) = load_histories(&config.scope);
    assert_eq!(history.path(), in_process.path());
    assert_eq!(
        global.as_ref().map(|g| g.path().to_path_buf()),
        in_process_global.map(|g| g.path().to_path_buf())
    );
    // What the UI loop does with a submitted line: append it to each.
    let submit = |line: &str,
                  history: &mut rkat_tui::ui::History,
                  global: &mut Option<rkat_tui::ui::History>| {
        history.append(line).unwrap();
        if let Some(global) = global {
            global.append(line).unwrap();
        }
    };
    submit("fix the parser", &mut history, &mut global);
    submit("run the tests", &mut history, &mut global);

    // The next attach-mode start.
    let (history, global) = load_histories(&config.scope);
    let mut app = build_app(
        &rkat_tui::client::app_setup(&config),
        StatusInfo::default(),
        history.entries(),
        global.as_ref().map(|g| g.entries()).unwrap_or_default(),
    );
    let key = |code| Msg::Key(KeyEvent::new(code, KeyModifiers::NONE));
    app.update(key(KeyCode::Up));
    assert_eq!(app.input.text(), "run the tests");
    app.update(key(KeyCode::Up));
    assert_eq!(app.input.text(), "fix the parser");
    app.update(key(KeyCode::Down));
    assert_eq!(app.input.text(), "run the tests");
    app.update(key(KeyCode::Down));
    assert_eq!(app.input.text(), "");
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    )));
    for c in "pars".chars() {
        app.update(key(KeyCode::Char(c)));
    }
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert_eq!(app.input.text(), "fix the parser");
}
