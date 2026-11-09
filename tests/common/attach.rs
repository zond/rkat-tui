//! The attach-mode rig shared by `tests/attach.rs`,
//! `tests/attach_approvals.rs`, `tests/attach_helpers.rs`,
//! `tests/attach_sessions.rs` and `tests/daemon.rs`: a daemon started
//! in-process with a scripted LLM (an allow-all classifier unless the test
//! passes its own options), and attach runners driven through the UI's
//! `Effect`/`Msg` channels. `tests/daemon.rs` keeps its own raw JSON-RPC
//! client, which tests the wire below the attach runner.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meerkat::LlmClient;
use meerkat_core::AgentEvent;
use rkat_tui::approval::classifier::{
    ActionClassifier, ClassifierError, ClassifierInput, ClassifierVerdict,
};
use rkat_tui::approval::escalation::RecordedPrompt;
use rkat_tui::approval::mode::PermissionMode;
use rkat_tui::bootstrap::{ApprovalOverrides, BootstrapOptions, HostPaths};
use rkat_tui::cli::AttachMode;
use rkat_tui::client::testing::{AttachHarness, spawn_attach_runner};
use rkat_tui::client::transport::{Connection, Notification};
use rkat_tui::daemon::approvals::DaemonToolOrigins;
use rkat_tui::daemon::endpoint::Endpoint;
use rkat_tui::daemon::{Daemon, DaemonError, DaemonOptions};
use rkat_tui::ui::{Effect, EscalationId, Msg, StatusUpdate, TurnOutcome};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::common::{ScriptedLlmClient, TIMEOUT, args_for, endpoint_file_for, no_lists, within};

/// Allows every reviewed call (these tests are not about the gate).
pub struct AllowAll;

#[async_trait::async_trait]
impl ActionClassifier for AllowAll {
    async fn classify(
        &self,
        _input: ClassifierInput<'_>,
    ) -> Result<ClassifierVerdict, ClassifierError> {
        Ok(ClassifierVerdict::Allow {
            reason: "test".into(),
        })
    }
}

/// A daemon serving in the background.
pub struct Running {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), DaemonError>>,
    pub endpoint_file: PathBuf,
    pub session: meerkat_core::SessionId,
    /// Whether the orchestrator was created (not restored) at start.
    pub created: bool,
    pub origins: Arc<DaemonToolOrigins>,
}

impl Running {
    /// The bootstrap of a test daemon: the scripted LLM, no model lists,
    /// the allow-all classifier.
    pub fn bootstrap(llm: &Arc<ScriptedLlmClient>) -> BootstrapOptions {
        BootstrapOptions {
            llm_override: Some(Arc::clone(llm) as Arc<dyn LlmClient>),
            lister: Some(no_lists()),
            host_paths: Some(HostPaths::default()),
            approval: ApprovalOverrides {
                classifier: Some(Arc::new(AllowAll) as Arc<dyn ActionClassifier>),
                escalation_timeout: None,
            },
        }
    }

    /// The options of a test daemon: [`Self::bootstrap`], its endpoint
    /// file outside the workspace.
    pub fn options(root: &Path, llm: &Arc<ScriptedLlmClient>) -> DaemonOptions {
        DaemonOptions::default()
            .with_bootstrap(Self::bootstrap(llm))
            .with_endpoint_file(endpoint_file_for(root))
    }

    pub async fn start(root: &Path, llm: &Arc<ScriptedLlmClient>) -> Self {
        Self::start_with(&args_for(root), Self::options(root, llm)).await
    }

    /// Start with these arguments and options.
    pub async fn start_with(args: &rkat_tui::Args, options: DaemonOptions) -> Self {
        let daemon = within(Daemon::start(args, options))
            .await
            .expect("daemon starts");
        Self::serve(daemon)
    }

    /// Serve a started daemon in the background.
    pub fn serve(daemon: Daemon) -> Self {
        let endpoint_file = daemon.endpoint_file().to_path_buf();
        let session = daemon.orchestrator().session_id.clone();
        let created = daemon.orchestrator().created;
        let origins = Arc::clone(daemon.tool_origins());
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(daemon.serve_until(async move {
            let _ = stopped.await;
        }));
        Self {
            stop,
            task,
            endpoint_file,
            session,
            created,
            origins,
        }
    }

    /// The daemon's endpoint.
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::read(&self.endpoint_file).unwrap()
    }

    /// A raw connection to the daemon (not initialized) and its
    /// notifications.
    pub async fn connect_with_notes(&self) -> (Connection, UnboundedReceiver<Notification>) {
        Connection::attach(&self.endpoint(), Duration::from_secs(5))
            .await
            .unwrap()
    }

    /// A raw connection to the daemon.
    pub async fn connect(&self) -> Connection {
        let (connection, _notes) = self.connect_with_notes().await;
        connection
            .call("initialize", serde_json::json!({}))
            .await
            .unwrap();
        connection
    }

    pub async fn stop(self) {
        let _ = self.stop.send(());
        within(self.task).await.unwrap().expect("clean shutdown");
    }
}

/// An attach runner and everything it said.
pub struct Client {
    pub harness: AttachHarness,
    pub seen: Vec<Msg>,
}

impl Client {
    pub async fn attach(root: &Path) -> Self {
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

    pub fn send(&self, effect: Effect) {
        self.harness.effects.send(effect).unwrap();
    }

    /// Wait for a message matching `pred` (seen before or arriving).
    pub async fn saw(&mut self, pred: impl Fn(&Msg) -> bool) {
        if !self.seen.iter().any(&pred) {
            self.wait_for(pred).await;
        }
    }

    pub async fn wait_for(&mut self, pred: impl Fn(&Msg) -> bool) -> Msg {
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
                let tail = &self.seen[self.seen.len().saturating_sub(20)..];
                panic!("timed out; the last messages: {tail:#?}")
            }
        }
    }

    pub async fn attached(&mut self) {
        self.wait_for(|msg| {
            matches!(msg, Msg::Status(StatusUpdate::Attach(Some(state))) if state.starts_with("attached"))
        })
        .await;
    }

    pub async fn turn_finished(&mut self) -> TurnOutcome {
        match self
            .wait_for(|msg| matches!(msg, Msg::TurnFinished(_)))
            .await
        {
            Msg::TurnFinished(outcome) => outcome,
            _ => unreachable!(),
        }
    }

    /// The agent events seen so far.
    pub fn events(&self) -> Vec<&AgentEvent> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                Msg::Agent(env) => Some(&env.payload),
                _ => None,
            })
            .collect()
    }

    /// How many `text_complete` events with `text` were seen.
    pub fn texts(&self, text: &str) -> usize {
        self.events()
            .iter()
            .filter(|event| matches!(event, AgentEvent::TextComplete { content, .. } if content == text))
            .count()
    }

    pub async fn escalation(&mut self) -> (RecordedPrompt, EscalationId) {
        match self
            .wait_for(|msg| matches!(msg, Msg::Escalation(..)))
            .await
        {
            Msg::Escalation(prompt, id) => (prompt, id),
            _ => unreachable!(),
        }
    }

    /// How many approval dialogs were shown.
    pub fn escalations(&self) -> usize {
        self.seen
            .iter()
            .filter(|msg| matches!(msg, Msg::Escalation(..)))
            .count()
    }

    pub async fn set_mode(&mut self, mode: PermissionMode) {
        self.send(Effect::SetMode(mode));
        self.wait_for(|msg| {
            matches!(msg, Msg::Status(StatusUpdate::Mode { mode: shown, .. }) if *shown == mode)
        })
        .await;
    }

    pub fn notices(&self) -> String {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                Msg::Notice { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Quit; where the messages the daemon never got were saved.
    pub async fn quit(self) -> Option<rkat_tui::runner::SavedUnsent> {
        self.send(Effect::Quit);
        within(self.harness.task).await.unwrap()
    }
}
