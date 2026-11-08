//! Attach mode: the TUI's attach runner (`rkat-tui --attach`) against the
//! `rkat-tui-mob` daemon started in-process with a scripted LLM, driven
//! through the same `Effect`/`Msg` channels the UI uses.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{ScriptedLlmClient, Step, TIMEOUT, Workspace, args_for, no_lists, within};
use meerkat::LlmClient;
use meerkat_core::AgentEvent;
use rkat_tui::bootstrap::{BootstrapOptions, HostPaths};
use rkat_tui::cli::AttachMode;
use rkat_tui::client::catchup::CursorFile;
use rkat_tui::client::runner::NOT_AVAILABLE;
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
        let options = DaemonOptions::default().with_bootstrap(BootstrapOptions {
            llm_override: Some(Arc::clone(llm) as Arc<dyn LlmClient>),
            lister: Some(no_lists()),
            host_paths: Some(HostPaths::default()),
            ..BootstrapOptions::default()
        });
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
        let config = rkat_tui::client::configure(&args_for(root), AttachMode::Existing)
            .await
            .unwrap();
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
    assert!(client.notices().contains("WITHOUT the approval gate"));
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
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("not a catalogued model")))
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
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_the_daemon_cannot_do_yet_says_so() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    for effect in [
        Effect::NewSession,
        Effect::RequestResumePicker,
        Effect::RequestRewindMenu,
        Effect::ListRules,
    ] {
        client.send(effect);
        client
            .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains(NOT_AVAILABLE)))
            .await;
    }
    client.send(Effect::CompactNow);
    client
        .wait_for(|msg| matches!(msg, Msg::CompactRebuilt(Err(_))))
        .await;
    assert!(
        client
            .notices()
            .contains("/new is not available in attach mode yet")
    );
    client.send(Effect::StatusReport);
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("attach mode") && text.contains("orchestrator `main`")))
        .await;
    client.quit().await;
    daemon.stop().await;
}
