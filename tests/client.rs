//! The client: its runner against the `rkat-tui-mob` daemon started inside the test process with a scripted
//! LLM, driven
//! through the same `Effect`/`Msg` channels the UI uses.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/rig.rs"]
mod rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, args_for, endpoint_file_for, no_lists, within};
use meerkat_core::AgentEvent;
use rig::{Client, Running};
use rkat_tui::cli::AttachTarget;
use rkat_tui::client::catchup::CursorFile;
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
    let mut config = rkat_tui::client::configure(&args_for(&ws.root), AttachTarget::Existing)
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
    assert_eq!(outcome.expect("completed").text, "answered");
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
    let summary = outcome.expect("completed");
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
    assert_eq!(outcome, Err(TurnFailure::Interrupted), "{:#?}", client.seen);
    // The member serves the next prompt.
    client.send(Effect::StartTurn("again".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "after");
    client.quit().await;
    daemon.stop().await;
}

/// `/model` in attach mode switches the session in place
/// (`rkat/set_model`): the session keeps the
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
    client.turn_finished().await.unwrap();
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
    assert_eq!(other.turn_finished().await.unwrap().text, "two");
    other.send(Effect::StartTurn("third".into()));
    assert_eq!(other.turn_finished().await.unwrap().text, "three");
    let models: Vec<String> = llm.recorded().into_iter().map(|r| r.model).collect();
    assert_eq!(
        models,
        vec!["claude-sonnet-4-6", "claude-opus-4-8", "claude-opus-4-8"]
    );
    other.quit().await;
    daemon.stop().await;
}

/// A model switch is refused while a turn runs.
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
    let _ = client.turn_finished().await;
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
    client.turn_finished().await.unwrap();
    client.send(Effect::StartTurn("long work".into()));
    client
        .wait_for(|msg| matches!(msg, Msg::Agent(env) if matches!(&env.payload, AgentEvent::RunStarted { input, .. } if input.prompt_text().as_deref() == Some("long work"))))
        .await;
    llm.wait_call(1).await;
    // Detach mid-turn; the turn goes on in the daemon.
    client.quit().await;
    gate.notify_one();
    let cursor = CursorFile::of_realm(&common::realm_of(&ws.root))
        .for_agent(&daemon.agent)
        .load()
        .expect("cursor saved");
    // The reattach resumes the stream from this position (same daemon).
    assert!(
        cursor.epoch.is_some() && cursor.daemon.is_some(),
        "{cursor:?}"
    );

    let mut client = Client::attach(&ws.root).await;
    let outcome = client.turn_finished().await;
    assert_eq!(outcome.unwrap().text, "finished without you");
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
    assert_eq!(client.turn_finished().await.unwrap().text, "first life");
    daemon.stop().await;
    client
        .wait_for(|msg| {
            matches!(
                msg,
                Msg::Status(StatusUpdate::Connection(
                    rkat_tui::ui::Connection::Reconnecting
                ))
            )
        })
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
    let cursor = CursorFile::of_realm(&common::realm_of(&ws.root)).for_agent(&daemon.agent);
    let before = cursor.load().unwrap();
    client.send(Effect::StartTurn("two".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "second life");
    assert_eq!(
        client.texts("first life"),
        1,
        "the catch-up did not repeat the first turn"
    );
    assert!(llm.recorded()[1].user().contains("two"));
    let after = cursor.load().unwrap();
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
    client.turn_finished().await.unwrap();
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
    let realm = common::realm_of(&ws.root).join("tui.toml");
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
/// daemon, the instruction files on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_reports_the_daemons_conversation() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("AGENTS.md"), "be brief\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("hi")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("hello".into()));
    client.turn_finished().await.unwrap();
    // The daemon reads the tool definitions once the agent is idle.
    let session = daemon.current_session().await;
    common::within(async {
        while daemon.origins.sizes(&session).is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
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
    client.quit().await;
    daemon.stop().await;
}

/// The agent's file changes show as diffs in every attached client (the
/// daemon pushes `rkat/file_diff`).
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
    client.turn_finished().await.unwrap();
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
/// a client when it first attaches.
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
    // A recall with no steer of this client outstanding takes nothing back
    // and is no error.
    client.send(Effect::RecallQueued);
    client
        .wait_for(|msg| matches!(msg, Msg::Recalled { recalled, errors } if recalled.is_empty() && errors.is_empty()))
        .await;
    client.send(Effect::StatusReport);
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("attached to the workspace daemon") && text.contains(&format!("agent `{}`", daemon.agent))))
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
    context.expect("the output joined the agent's context");
    client.send(Effect::StartTurn("what did I run?".into()));
    client.turn_finished().await.unwrap();
    let request = format!("{:?}", llm.requests()[0]);
    assert!(request.contains("bang-42"), "the agent read the output");
    client.quit().await;
    daemon.stop().await;
}

/// `--attach=DIRECTORY` from another workspace: the client attaches to
/// that workspace's daemon, and what it resolves locally (the `!` shell's
/// directory, `@` mentions) resolves against the daemon's workspace, not
/// the directory the client was started in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_to_a_directory_resolves_locally_against_its_workspace() {
    let served = Workspace::new();
    let elsewhere = Workspace::new();
    std::fs::write(served.root.join("only-here.txt"), "alpha-content\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("read it")]);
    let daemon = Running::start(&served.root, &llm).await;
    // Started in the other workspace, pointed at a subdirectory of the
    // served one: the workspace above it is found.
    let sub = served.root.join("src");
    std::fs::create_dir_all(&sub).unwrap();
    // The same user, so the same config directory: the served
    // workspace's realm is found from it by its manifest.
    let cfg = common::config_dir_for(&served.root);
    let args = common::args_with(&elsewhere.root, &["--config-dir", cfg.to_str().unwrap()]);
    let mut config = rkat_tui::client::configure(&args, AttachTarget::Directory(sub))
        .await
        .unwrap();
    let canonical = std::fs::canonicalize(&served.root).unwrap();
    assert_eq!(config.scope.context_root, canonical);
    config.lister = no_lists();
    config.endpoint_files = vec![endpoint_file_for(&served.root)];
    let mut client = Client {
        harness: spawn_attach_runner(config),
        seen: Vec::new(),
    };
    client.attached().await;
    client.send(Effect::RunShell {
        id: rkat_tui::ui::ShellRunId(1),
        command: "pwd".into(),
    });
    let finished = client
        .wait_for(|msg| matches!(msg, Msg::ShellFinished { .. }))
        .await;
    let Msg::ShellFinished {
        outcome: rkat_tui::ui::ShellOutcome::Ran { run, .. },
        ..
    } = finished
    else {
        panic!("{finished:?}");
    };
    assert_eq!(run.output.trim(), canonical.display().to_string());
    client.send(Effect::StartTurn("look at @only-here.txt".into()));
    client.turn_finished().await.unwrap();
    let request = format!("{:?}", llm.requests()[0]);
    assert!(
        request.contains("alpha-content"),
        "the mention was expanded there"
    );
    client.quit().await;
    daemon.stop().await;
}

/// A token file for `endpoint`, mode 0600, in `dir`.
#[cfg(unix)]
fn token_file(dir: &std::path::Path, token: &str, mode: u32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let file = dir.join("daemon.token");
    std::fs::write(&file, format!("{token}\n")).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
    file
}

/// `--attach=HOST:PORT` with `--attach-token-file`: the client reaches
/// the daemon by its address (as through a tunnel), presents the token
/// and attaches; `rkat/status` names the workspace and host. On this
/// machine the workspace is local and the client runs on it; when the
/// daemon is elsewhere (`remote`), `!`, `@` mentions, `/remember`,
/// `/memory` and `/export` to a path are refused with the reason, and
/// prompts still work.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_by_address_with_a_token_file() {
    let served = Workspace::new();
    let elsewhere = Workspace::new();
    std::fs::write(served.root.join("a.txt"), "alpha-content\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("local"), Step::Text("remote")]);
    let daemon = Running::start(&served.root, &llm).await;
    let endpoint = daemon.endpoint();
    let secrets = tempfile::tempdir().unwrap();
    let token = token_file(secrets.path(), &endpoint.token, 0o600);
    let at = format!("--attach={}", endpoint.address);
    let args = common::args_with(
        &elsewhere.root,
        &[&at, "--attach-token-file", token.to_str().unwrap()],
    );
    let target = args.attach.clone().unwrap();
    assert_eq!(target, AttachTarget::Address(endpoint.address.to_string()));
    // This machine's endpoint file names the daemon: local, run on the
    // daemon's workspace.
    let local_files = [daemon.endpoint_file.clone()];
    let mut config = rkat_tui::client::configure_at(&args, target.clone(), &local_files)
        .await
        .unwrap();
    assert_eq!(config.remote, None);
    assert_eq!(
        config.scope.context_root,
        std::fs::canonicalize(&served.root).unwrap()
    );
    assert_eq!(config.address.as_ref().unwrap().pid, std::process::id());
    assert!(config.spawn.is_none());
    config.lister = no_lists();
    let mut client = Client {
        harness: spawn_attach_runner(config),
        seen: Vec::new(),
    };
    client.attached().await;
    client.send(Effect::StartTurn("hello by address".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "local");
    client.quit().await;

    // The same host name and an existing path, but no endpoint file here
    // names it (a twin machine): remote, the file features refused.
    let mut config = rkat_tui::client::configure_at(&args, target, &[])
        .await
        .unwrap();
    let why = config.remote.clone().unwrap();
    assert!(why.contains("no endpoint file on this machine"), "{why}");
    assert!(config.endpoint_files.is_empty());
    assert_eq!(
        config.scope.context_root,
        std::fs::canonicalize(&elsewhere.root).unwrap()
    );
    config.lister = no_lists();
    let mut client = Client {
        harness: spawn_attach_runner(config),
        seen: Vec::new(),
    };
    client.attached().await;
    client.send(Effect::RunShell {
        id: rkat_tui::ui::ShellRunId(1),
        command: "pwd".into(),
    });
    let refused = client
        .wait_for(|msg| matches!(msg, Msg::ShellFinished { .. }))
        .await;
    assert!(
        matches!(&refused, Msg::ShellFinished { outcome: rkat_tui::ui::ShellOutcome::Failed(why), .. } if why.contains("no endpoint file on this machine")),
        "{refused:?}"
    );
    client.send(Effect::Remember("a note".into()));
    client
        .wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("/remember") && text.contains("no endpoint file on this machine")))
        .await;
    client.send(Effect::Export {
        path: Some("out.txt".into()),
        text: "x".into(),
    });
    client
        .wait_for(
            |m| matches!(m, Msg::Notice { text, .. } if text.starts_with("/export to a path")),
        )
        .await;
    client.send(Effect::StartTurn("look at @a.txt".into()));
    assert_eq!(client.turn_finished().await.unwrap().text, "remote");
    assert!(client.notices().contains("@ mentions were sent as typed"));
    let request = format!("{:?}", llm.requests()[1]);
    assert!(request.contains("@a.txt") && !request.contains("alpha-content"));
    client.quit().await;
    daemon.stop().await;
}

/// The token file must be private, and given.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_by_address_refuses_a_readable_or_missing_token_file() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let endpoint = daemon.endpoint();
    let secrets = tempfile::tempdir().unwrap();
    let token = token_file(secrets.path(), &endpoint.token, 0o644);
    let at = format!("--attach={}", endpoint.address);
    let args = common::args_with(
        &ws.root,
        &[&at, "--attach-token-file", token.to_str().unwrap()],
    );
    let refused = rkat_tui::client::configure(&args, args.attach.clone().unwrap())
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("chmod 600"), "{refused}");
    let args = common::args_with(&ws.root, &[&at]);
    let refused = rkat_tui::client::configure(&args, args.attach.clone().unwrap())
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("needs --attach-token-file"),
        "{refused}"
    );
    // A wrong token: no daemon answers with it.
    let wrong = token_file(secrets.path(), "0000", 0o600);
    let args = common::args_with(
        &ws.root,
        &[&at, "--attach-token-file", wrong.to_str().unwrap()],
    );
    let refused = rkat_tui::client::configure(&args, args.attach.clone().unwrap())
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("no daemon answered"),
        "{refused}"
    );
    daemon.stop().await;
}

/// The workspace's daemon is the default; `attach` in a `tui.toml` (which
/// chose the removed in-process runner) is no key any more: a file that
/// sets it is refused naming the file, as any unknown key is.
#[test]
fn the_daemon_is_the_default_and_a_tui_toml_attach_key_is_refused() {
    use rkat_tui::bootstrap::read_layers;
    let ws = Workspace::new();
    let args = args_for(&ws.root);
    assert_eq!(args.attach_target(), AttachTarget::Auto);
    assert!(read_layers(&args).is_ok());
    assert_eq!(
        common::args_with(&ws.root, &["--attach=existing"]).attach_target(),
        AttachTarget::Existing
    );
    let user = common::config_dir_for(&ws.root);
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(user.join("tui.toml"), "attach = false\n").unwrap();
    let refused = read_layers(&args).map(|_| ()).unwrap_err().to_string();
    assert!(
        refused.contains(user.join("tui.toml").to_str().unwrap()),
        "{refused}"
    );
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
    assert_eq!(b.turn_finished().await.unwrap().text, "third answer");
    a.wait_for(|m| matches!(m, Msg::RemoteTurn { prompt: Some(p) } if p == "from B"))
        .await;
    a.turn_finished().await.unwrap();
    for client in [&a, &b] {
        for text in ["first answer", "second answer", "third answer"] {
            assert_eq!(client.texts(text), 1, "{text}");
        }
    }
    a.quit().await;
    b.quit().await;
    daemon.stop().await;
}

/// Whether `msg` is the agent's `boundary_append_applied` carrying `text`.
fn applies(msg: &Msg, text: &str) -> bool {
    matches!(msg, Msg::Agent(env) if matches!(&env.payload,
        AgentEvent::BoundaryAppendApplied { content, .. } if content.text_content().contains(text)))
}

/// Two clients steer one running turn (lukacf/meerkat#2155): each marks
/// its own steer delivered at the boundary event of the input its send
/// admitted (or, when the runtime runs it as a follow-up run, at that
/// run's start), and never at the other's. The runner sends a steer's
/// `SteerDelivered` just before the event that delivered it. The engine
/// applies A's steer (admitted first) at the tool boundary and runs B's
/// as a follow-up run, so a first-in-first-out client would have B mark
/// its own steer at A's boundary event: the negative case.
///
/// The follow-up run is part of the turn for both clients: a client ends
/// its turn only when every run it saw ended and the daemon reports no
/// unfinished input of the conversation (`rkat/active_inputs`), so a steer
/// still queued (whoever sent it) keeps the turn open for the run that
/// carries it, however late that run starts, and each client sees one turn
/// end, after its mark. The client that did not send the follow-up's steer
/// shows its text inside the turn. A short run-grace timer decided this
/// before, and a slow host ended the turn first (once for B, sweeping its
/// steer "delivered", once for A, which showed B's run as a new turn).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_client_marks_only_its_own_steer_delivered() {
    use rkat_tui::ui::delivery::PendingId;
    let ws = Workspace::new();
    std::fs::write(ws.root.join("notes.txt"), "the answer is 42\n").unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(
            Arc::clone(&gate),
            Box::new(Step::Tool {
                id: "call-1",
                name: "read_file",
                args: serde_json::json!({ "path": "notes.txt" }),
            }),
        ),
        Step::Text("done"),
        Step::Text("follow-up"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = Client::attach(&ws.root).await;
    let mut b = Client::attach(&ws.root).await;
    a.send(Effect::StartTurn("read the notes".into()));
    llm.wait_call(0).await;
    b.wait_for(|m| matches!(m, Msg::RemoteTurn { prompt: Some(p) } if p == "read the notes"))
        .await;
    for (client, id, text) in [(&mut a, 1, "steer from A"), (&mut b, 2, "steer from B")] {
        client.send(Effect::Steer {
            id: PendingId(id),
            text: text.into(),
        });
        let ack = client.wait_for(|m| matches!(m, Msg::SteerAck { .. })).await;
        assert!(
            matches!(ack, Msg::SteerAck { id: acked, result: Ok(()) } if acked == PendingId(id)),
            "{ack:?}"
        );
    }
    gate.notify_one();
    // The positive barrier: each saw its own delivery and the work's end
    // (both steers reached the model).
    a.wait_for(|m| matches!(m, Msg::SteerDelivered(PendingId(1))))
        .await;
    b.wait_for(|m| matches!(m, Msg::SteerDelivered(PendingId(2))))
        .await;
    llm.wait_call(2).await;
    for client in [&mut a, &mut b] {
        client
            .wait_for(|m| {
                matches!(m, Msg::Agent(env) if matches!(&env.payload,
                AgentEvent::TextComplete { content, .. } if content == "follow-up"))
            })
            .await;
        client.turn_finished().await.unwrap();
    }
    let last = common::user_texts(&llm.recorded()[2].messages);
    assert!(
        ["steer from A", "steer from B"]
            .iter()
            .all(|text| last.iter().any(|t| t == text)),
        "{last:?}"
    );
    let mut negative_seen = false;
    for (client, own, mine, theirs) in [
        (&a, PendingId(1), "steer from A", "steer from B"),
        (&b, PendingId(2), "steer from B", "steer from A"),
    ] {
        let seen = &client.seen;
        let marks: Vec<usize> = seen
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(m, Msg::SteerDelivered(_)))
            .map(|(at, _)| at)
            .collect();
        assert_eq!(marks.len(), 1, "one mark, its own: {seen:#?}");
        let at = marks[0];
        assert!(matches!(seen[at], Msg::SteerDelivered(id) if id == own));
        let follow_up = matches!(&seen[at + 1], Msg::Agent(env)
            if matches!(env.payload, AgentEvent::RunStarted { .. }));
        assert!(
            applies(&seen[at + 1], mine) || follow_up,
            "marked at its own input's event or its follow-up run: {seen:#?}"
        );
        let ends: Vec<usize> = seen
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(m, Msg::TurnFinished(_)))
            .map(|(at, _)| at)
            .collect();
        assert_eq!(ends.len(), 1, "one turn, ended once: {seen:#?}");
        assert!(ends[0] > at, "the turn ends after the mark: {seen:#?}");
        if !follow_up {
            // The other client's steer joined this client's turn as its
            // follow-up run: its text is shown inside the turn.
            assert!(
                seen[at..ends[0]]
                    .iter()
                    .any(|m| matches!(m, Msg::RemoteTurn { prompt: Some(p) } if p == theirs)),
                "the other client's steer shown in the turn: {seen:#?}"
            );
        }
        if let Some(theirs_at) = seen.iter().position(|m| applies(m, theirs)) {
            negative_seen = true;
            assert!(
                !matches!(seen[theirs_at - 1], Msg::SteerDelivered(_)),
                "nothing marked at the other client's input: {seen:#?}"
            );
        }
    }
    assert!(
        negative_seen,
        "a client saw the other's steer applied at a boundary"
    );
    a.quit().await;
    b.quit().await;
    daemon.stop().await;
}

/// The daemon names the input it admitted for a prompt into an idle agent
/// and for a steer into a running turn (`admission.input_id`): the client
/// attributes steers by it, so an engine that stopped naming it must fail
/// here rather than mis-mark steers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn member_send_names_the_admitted_input() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&gate), Box::new(Step::Text("first answer"))),
        Step::Text("second answer"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    let prompt = rkat_tui::client::remote::send(&connection, "hello", false)
        .await
        .unwrap();
    assert!(prompt.is_some(), "a prompt into an idle agent is named");
    llm.wait_call(0).await;
    let steer = rkat_tui::client::remote::send(&connection, "and this", true)
        .await
        .unwrap();
    assert!(steer.is_some(), "a steer into a running turn is named");
    assert_ne!(prompt, steer);
    gate.notify_one();
    llm.wait_call(1).await;
    drop(connection);
    daemon.stop().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reconnect_restarts_a_daemon_that_died() {
    let ws = Workspace::new();
    // The real daemon below needs an endpoint with a key (any variable that
    // is set: it never calls the endpoint).
    let user = common::config_dir_for(&ws.root);
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(
        user.join("tui.toml"),
        "[endpoints.fake]\ndialect = \"anthropic\"\nbase_url = \"http://127.0.0.1:9\"\napi_key_env = \"HOME\"\n",
    )
    .unwrap();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let args = args_for(&ws.root);
    let mut config = rkat_tui::client::configure(&args, AttachTarget::Auto)
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
    // (A crash leaves the file in its directory; the stop removed both.)
    std::fs::create_dir_all(endpoint_file.parent().unwrap()).unwrap();
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
    // Without a connection there is no one to ask: `/daemon stop` says
    // so and names the way to stop a daemon by hand; nothing is signalled.
    let attached = |client: &Client| {
        client.seen.iter().rev().find_map(|msg| match msg {
            Msg::Status(rkat_tui::ui::StatusUpdate::Connection(state)) => {
                Some(matches!(state, rkat_tui::ui::Connection::Attached { .. }))
            }
            _ => None,
        }) == Some(true)
    };
    while attached(&client) {
        client
            .wait_for(|m| matches!(m, Msg::Status(rkat_tui::ui::StatusUpdate::Connection(_))))
            .await;
    }
    client.send(Effect::DaemonStop);
    client
        .wait_for(|m| {
            matches!(m, Msg::Notice { text, .. }
                if text.contains("cannot be asked to stop") && text.contains("kill <pid>"))
        })
        .await;
    client.quit().await;
}

/// The findings this guards: attach mode never pushed prices (`/cost`
/// showed `$?`) and its `!` hint said "sandbox" under `--no-confinement`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_mode_prices_the_cost_readout_and_says_the_shell_runs_as_you() {
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
    let mut config = rkat_tui::client::configure(&args, AttachTarget::Existing)
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
    assert!(app.shell_mode_hint().contains("as you, no sandbox"));
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

/// The client builds its UI from the configuration it read at start
/// (`client::app_setup` over the `tui.toml` layers, then
/// `workbench::build_app`): the `[ui]` and `[auto]` settings and the `!`
/// shell's confinement reach the app. (Until P4 this compared the setup
/// with the in-process runner's, which drifted from it once.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_client_builds_its_app_from_the_config() {
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
            // Below the warning: clamped up to 60.
            "--set",
            "auto.compact_auto_percent=50",
        ],
    );
    let config = rkat_tui::client::configure(&args, AttachTarget::Existing)
        .await
        .unwrap();
    let setup = rkat_tui::client::app_setup(&config);
    assert!(!setup.ui.bell);
    assert_eq!(
        (setup.compaction.warn_percent, setup.compaction.auto_percent),
        (60, 60)
    );
    let app = build_app(&setup, StatusInfo::default(), &[], &[]);
    assert_eq!(app.compaction_policy(), setup.compaction);
    assert!(app.shell_mode_hint().contains("as you, no sandbox"));
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

/// ↑/↓ recall and Ctrl+R search in attach mode: the attach runner's UI
/// loads the history files of the workspace's realm and of the user (the
/// scope the daemon resolves too), a line submitted is appended to them,
/// and the next start recalls it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_mode_recalls_and_searches_the_shared_input_history() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use rkat_tui::workbench::app::load_histories;

    let ws = Workspace::new();
    let args = args_for(&ws.root);
    let layers = rkat_tui::bootstrap::read_layers(&args).unwrap();
    let (resolved, resolved_global) = load_histories(&layers.scope);
    let config = rkat_tui::client::configure(&args, AttachTarget::Existing)
        .await
        .unwrap();
    let (mut history, mut global) = load_histories(&config.scope);
    assert_eq!(history.path(), resolved.path());
    assert_eq!(
        global.as_ref().map(|g| g.path().to_path_buf()),
        resolved_global.map(|g| g.path().to_path_buf())
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
