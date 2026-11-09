//! Attach mode: the TUI's attach runner (`rkat-tui --attach`) against the
//! `rkat-tui-mob` daemon started in-process with a scripted LLM, driven
//! through the same `Effect`/`Msg` channels the UI uses.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{
    ScriptedLlmClient, Step, TIMEOUT, Workspace, args_for, endpoint_file_for, no_lists, within,
};
use meerkat::LlmClient;
use meerkat_core::AgentEvent;
use rkat_tui::bootstrap::{BootstrapOptions, HostPaths};
use rkat_tui::cli::AttachMode;
use rkat_tui::client::catchup::CursorFile;
use rkat_tui::client::runner::NOT_AVAILABLE;
use rkat_tui::client::sessions::REWIND_UNAVAILABLE;
use rkat_tui::client::testing::{AttachHarness, spawn_attach_runner};
use rkat_tui::daemon::{Daemon, DaemonError, DaemonOptions};
use rkat_tui::ui::{Effect, Msg, StatusUpdate, TurnFailure, TurnOutcome};

/// A daemon serving in the background.
struct Running {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), DaemonError>>,
}

impl Running {
    async fn start(root: &Path, llm: &Arc<ScriptedLlmClient>) -> Self {
        let options = DaemonOptions::default()
            .with_bootstrap(BootstrapOptions {
                llm_override: Some(Arc::clone(llm) as Arc<dyn LlmClient>),
                lister: Some(no_lists()),
                host_paths: Some(HostPaths::default()),
                ..BootstrapOptions::default()
            })
            .with_endpoint_file(endpoint_file_for(root));
        let daemon = within(Daemon::start(&args_for(root), options))
            .await
            .expect("daemon starts");
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(daemon.serve_until(async move {
            let _ = stopped.await;
        }));
        Self { stop, task }
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        within(self.task).await.unwrap().expect("clean shutdown");
    }
}

/// An attach runner and everything it said.
struct Client {
    harness: AttachHarness,
    seen: Vec<Msg>,
}

impl Client {
    async fn attach(root: &Path) -> Self {
        let mut config = rkat_tui::client::configure(&args_for(root), AttachMode::Existing)
            .await
            .unwrap();
        config.lister = no_lists();
        config.endpoint_file = endpoint_file_for(root);
        let mut client = Self {
            harness: spawn_attach_runner(config),
            seen: Vec::new(),
        };
        client.attached().await;
        client
    }

    fn send(&self, effect: Effect) {
        self.harness.effects.send(effect).unwrap();
    }

    async fn wait_for(&mut self, pred: impl Fn(&Msg) -> bool) -> Msg {
        let seen = &mut self.seen;
        let rx = &mut self.harness.ui_rx;
        let found = tokio::time::timeout(TIMEOUT, async {
            loop {
                let msg = rx.recv().await.expect("runner gone");
                seen.push(msg.clone());
                if pred(&msg) {
                    return msg;
                }
            }
        })
        .await;
        match found {
            Ok(msg) => msg,
            Err(_) => {
                let tail = &self.seen[self.seen.len().saturating_sub(15)..];
                panic!("timed out; the last messages: {tail:#?}")
            }
        }
    }

    async fn attached(&mut self) {
        self.wait_for(|msg| {
            matches!(msg, Msg::Status(StatusUpdate::Attach(Some(state))) if state.starts_with("attached"))
        })
        .await;
    }

    async fn turn_finished(&mut self) -> TurnOutcome {
        match self
            .wait_for(|msg| matches!(msg, Msg::TurnFinished(_)))
            .await
        {
            Msg::TurnFinished(outcome) => outcome,
            _ => unreachable!(),
        }
    }

    /// The agent events seen so far.
    fn events(&self) -> Vec<&AgentEvent> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                Msg::Agent(env) => Some(&env.payload),
                _ => None,
            })
            .collect()
    }

    /// How many `text_complete` events with `text` were seen.
    fn texts(&self, text: &str) -> usize {
        self.events()
            .iter()
            .filter(|event| matches!(event, AgentEvent::TextComplete { content, .. } if content == text))
            .count()
    }

    fn notices(&self) -> String {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                Msg::Notice { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn quit(self) {
        self.send(Effect::Quit);
        within(self.harness.task).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_is_answered_and_rendered() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("hello from the daemon")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    assert!(client.notices().contains("behind the same approval gate"));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_switches_with_the_next_prompt_and_stays() {
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
        .wait_for(|msg| matches!(msg, Msg::Status(StatusUpdate::Model { model, .. }) if model == "claude-opus-4-8"))
        .await;
    client.send(Effect::StartTurn("second".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "two");
    client.send(Effect::StartTurn("third".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "three");
    let models: Vec<String> = llm.recorded().into_iter().map(|r| r.model).collect();
    assert_eq!(
        models,
        vec!["claude-sonnet-4-6", "claude-opus-4-8", "claude-opus-4-8"]
    );
    // A model this mode cannot name is refused before anything is sent.
    client.send(Effect::SetModel("my-alias".into()));
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("cannot tell which endpoint serves `my-alias`")))
        .await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_the_daemon_cannot_do_yet_says_so() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::RecallQueued);
    client
        .wait_for(|msg| matches!(msg, Msg::Recalled { errors, .. } if errors.iter().any(|e| e.contains(NOT_AVAILABLE))))
        .await;
    client.send(Effect::RequestRewindMenu);
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text == REWIND_UNAVAILABLE))
        .await;
    client.send(Effect::StatusReport);
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("attach mode") && text.contains("orchestrator `main`")))
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
fn attach_true_in_a_users_tui_toml_selects_attach_mode() {
    use rkat_tui::bootstrap::attach_mode;
    let ws = Workspace::new();
    let args = args_for(&ws.root);
    assert_eq!(attach_mode(&args).unwrap(), None);
    // A user layer (the --config-dir tui.toml) turns it on.
    let user = ws.root.join(".cfg");
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(user.join("tui.toml"), "attach = true\n").unwrap();
    assert_eq!(attach_mode(&args).unwrap(), Some(AttachMode::Auto));
    // The flags win.
    assert_eq!(
        attach_mode(&common::args_with(&ws.root, &["--no-attach"])).unwrap(),
        None
    );
    assert_eq!(
        attach_mode(&common::args_with(&ws.root, &["--attach=existing"])).unwrap(),
        Some(AttachMode::Existing)
    );
    // A project file cannot choose it.
    std::fs::remove_file(user.join("tui.toml")).unwrap();
    std::fs::create_dir_all(ws.root.join(".rkat")).unwrap();
    std::fs::write(ws.root.join(".rkat/tui.toml"), "attach = true\n").unwrap();
    assert_eq!(attach_mode(&args).unwrap(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_steer_in_turn_and_both_see_the_whole_conversation() {
    use rkat_tui::delivery::PendingId;
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
    config.endpoint_file = endpoint_file_for(&ws.root);
    if let Some(plan) = &mut config.spawn {
        plan.program = std::path::PathBuf::from(env!("CARGO_BIN_EXE_rkat-tui-mob"));
    }
    let endpoint_file = config.endpoint_file.clone();
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

/// The upgrade path in the client: a daemon of an earlier version that
/// still answers at the legacy endpoint file inside the workspace is not
/// joined by a second daemon; the client says so and offers `/daemon stop`,
/// which stops it, waits for it to exit, and then starts the current daemon
/// and attaches. The stale legacy file is removed on the way.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_legacy_daemon_is_stopped_on_request_and_then_replaced() {
    let ws = Workspace::new();
    let user = ws.root.join(".cfg");
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(
        user.join("tui.toml"),
        "[endpoints.fake]\ndialect = \"anthropic\"\nbase_url = \"http://127.0.0.1:9\"\napi_key_env = \"HOME\"\n",
    )
    .unwrap();
    // The "earlier daemon": a process to signal and a socket that answers.
    let mut old = tokio::process::Command::new("sleep")
        .arg("120")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = old.id().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let legacy = rkat_tui::daemon::endpoint::legacy_file(&ws.root);
    rkat_tui::daemon::endpoint::Endpoint {
        address: listener.local_addr().unwrap(),
        token: "old".into(),
        pid,
    }
    .write(&legacy)
    .unwrap();

    let mut config = rkat_tui::client::configure(&args_for(&ws.root), AttachMode::Auto)
        .await
        .unwrap();
    config.lister = no_lists();
    config.endpoint_file = endpoint_file_for(&ws.root);
    if let Some(plan) = &mut config.spawn {
        plan.program = std::path::PathBuf::from(env!("CARGO_BIN_EXE_rkat-tui-mob"));
    }
    let endpoint_file = config.endpoint_file.clone();
    let mut client = Client {
        harness: spawn_attach_runner(config),
        seen: Vec::new(),
    };
    let refused = client
        .wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.contains("earlier version")))
        .await;
    let Msg::Notice { text, .. } = refused else {
        unreachable!()
    };
    assert!(
        text.contains(&format!("pid {pid}")) && text.contains("/daemon stop"),
        "{text}"
    );
    assert!(!endpoint_file.exists(), "no second daemon was started");

    client.send(Effect::DaemonStop);
    client
        .wait_for(
            |m| matches!(m, Msg::Notice { text, .. } if text.contains("asked the earlier daemon")),
        )
        .await;
    drop(listener);
    let status = within(old.wait()).await.unwrap();
    assert!(!status.success(), "SIGTERM ended it: {status}");
    client.attached().await;
    assert!(
        !legacy.exists(),
        "the stale legacy endpoint file is removed"
    );
    assert!(endpoint_file.exists());

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
