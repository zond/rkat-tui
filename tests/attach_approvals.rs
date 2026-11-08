//! Approvals in attach mode (P3): the daemon's orchestrator and its
//! helpers run behind the TUI's approval engine, the human answers in the
//! attached TUI over the engine's `approval/*` methods, the permission mode
//! and the rules reach the daemon through its `rkat/*` methods, and the
//! deny rules are the members' durable application tool policy. The daemon
//! runs in-process with a scripted LLM and a scripted classifier; the
//! clients are attach runners driven through the UI's channels.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::{
    ScriptedLlmClient, Step, TIMEOUT, Workspace, args_for, endpoint_file_for, no_lists, within,
};
use meerkat::LlmClient;
use meerkat_core::Message;
use meerkat_core::types::ToolResult;
use parking_lot::Mutex;
use rkat_tui::approval::classifier::{
    ActionClassifier, ClassifierError, ClassifierInput, ClassifierVerdict,
};
use rkat_tui::approval::mode::PermissionMode;
use rkat_tui::bootstrap::{ApprovalOverrides, BootstrapOptions, HostPaths};
use rkat_tui::cli::AttachMode;
use rkat_tui::client::testing::{AttachHarness, spawn_attach_runner};
use rkat_tui::client::transport::Connection;
use rkat_tui::daemon::endpoint::Endpoint;
use rkat_tui::daemon::{Daemon, DaemonError, DaemonOptions};
use rkat_tui::ui::{
    Effect, EscalationAnswer, EscalationId, EscalationPrompt, Msg, RuleEffect, StatusUpdate,
    TurnOutcome,
};
use serde_json::{Value, json};

/// A classifier that replays verdicts (allow when the script is empty).
#[derive(Default)]
struct ScriptedClassifier {
    verdicts: Mutex<Vec<ClassifierVerdict>>,
    seen: Mutex<Vec<String>>,
    /// The tool of every action it was asked about.
    tools: Mutex<Vec<String>>,
}

#[async_trait]
impl ActionClassifier for ScriptedClassifier {
    async fn classify(
        &self,
        input: ClassifierInput<'_>,
    ) -> Result<ClassifierVerdict, ClassifierError> {
        self.seen.lock().push(input.transcript.render());
        self.tools.lock().push(input.tool.to_string());
        let mut verdicts = self.verdicts.lock();
        if verdicts.is_empty() {
            Ok(ClassifierVerdict::Allow {
                reason: "scripted allow".into(),
            })
        } else {
            Ok(verdicts.remove(0))
        }
    }
}

/// How a test daemon is started.
#[derive(Clone, Default)]
struct Setup {
    classifier: Arc<ScriptedClassifier>,
    escalation_timeout: Option<Duration>,
    without_registry: bool,
    /// The user config root outside the workspace (so the workspace's
    /// `.rkat/config.toml` is a project input, not the user's own file).
    user_root_outside: bool,
}

impl Setup {
    fn options(&self, llm: &Arc<ScriptedLlmClient>) -> DaemonOptions {
        let options = DaemonOptions::default().with_bootstrap(BootstrapOptions {
            llm_override: Some(Arc::clone(llm) as Arc<dyn LlmClient>),
            lister: Some(no_lists()),
            host_paths: Some(HostPaths::default()),
            approval: ApprovalOverrides {
                classifier: Some(Arc::clone(&self.classifier) as Arc<dyn ActionClassifier>),
                escalation_timeout: self.escalation_timeout,
            },
        });
        if self.without_registry {
            options.without_policy_registry()
        } else {
            options
        }
    }
}

/// A daemon serving in the background.
struct Running {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), DaemonError>>,
    endpoint_file: std::path::PathBuf,
    origins: Arc<rkat_tui::daemon::approvals::DaemonToolOrigins>,
}

impl Running {
    async fn start(root: &Path, llm: &Arc<ScriptedLlmClient>, setup: &Setup) -> Self {
        let daemon = within(Self::try_start(root, llm, setup))
            .await
            .expect("daemon starts");
        Self::serve(daemon)
    }

    async fn try_start(
        root: &Path,
        llm: &Arc<ScriptedLlmClient>,
        setup: &Setup,
    ) -> Result<Daemon, DaemonError> {
        let args = if setup.user_root_outside {
            args_with_user_root(root)
        } else {
            args_for(root)
        };
        Daemon::start(
            &args,
            setup
                .options(llm)
                .with_endpoint_file(endpoint_file_for(root)),
        )
        .await
    }

    fn serve(daemon: Daemon) -> Self {
        let endpoint_file = daemon.endpoint_file().to_path_buf();
        let origins = Arc::clone(daemon.tool_origins());
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(daemon.serve_until(async move {
            let _ = stopped.await;
        }));
        Self {
            stop,
            task,
            endpoint_file,
            origins,
        }
    }

    /// A raw connection to the daemon.
    async fn connect(&self) -> Connection {
        let endpoint = Endpoint::read(&self.endpoint_file).unwrap();
        let (connection, _notes) = Connection::attach(&endpoint, Duration::from_secs(5))
            .await
            .unwrap();
        connection
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
        config.endpoint_file = endpoint_file_for(root);
        let mut client = Self {
            harness: spawn_attach_runner(config),
            seen: Vec::new(),
        };
        client
            .wait_for(|msg| {
                matches!(msg, Msg::Status(StatusUpdate::Attach(Some(state))) if state.starts_with("attached"))
            })
            .await;
        // The daemon's mode reaches the status bar on attach.
        client
            .wait_for(|msg| matches!(msg, Msg::Status(StatusUpdate::Mode { .. })))
            .await;
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
                let tail = &self.seen[self.seen.len().saturating_sub(20)..];
                panic!("timed out; the last messages: {tail:#?}")
            }
        }
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

    async fn escalation(&mut self) -> (EscalationPrompt, EscalationId) {
        match self
            .wait_for(|msg| matches!(msg, Msg::Escalation(..)))
            .await
        {
            Msg::Escalation(prompt, id) => (prompt, id),
            _ => unreachable!(),
        }
    }

    fn escalations(&self) -> usize {
        self.seen
            .iter()
            .filter(|msg| matches!(msg, Msg::Escalation(..)))
            .count()
    }

    async fn set_mode(&mut self, mode: PermissionMode) {
        self.send(Effect::SetMode(mode));
        self.wait_for(|msg| {
            matches!(msg, Msg::Status(StatusUpdate::Mode { mode: shown, .. }) if *shown == mode)
        })
        .await;
    }

    async fn quit(self) {
        self.send(Effect::Quit);
        within(self.harness.task).await.unwrap();
    }
}

fn tool_result(messages: &[Message], id: &str) -> ToolResult {
    messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResults { results, .. } => {
                results.iter().find(|r| r.tool_use_id == id).cloned()
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

/// The text of the tool result `id` the model read in request `n`.
fn result_text(llm: &ScriptedLlmClient, n: usize, id: &str) -> String {
    tool_result(&llm.requests()[n], id).text_content()
}

/// [`args_for`] with the user config root beside the workspace.
fn args_with_user_root(root: &Path) -> rkat_tui::Args {
    use clap::Parser as _;
    let home = root.parent().unwrap().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let root = root.to_str().unwrap();
    let cfg = format!("{root}/.cfg");
    rkat_tui::Args::try_parse_from([
        "rkat-tui",
        "-w",
        root,
        "--user-config-root",
        home.to_str().unwrap(),
        "--config-dir",
        &cfg,
        "--no-pricing-fetch",
        "--model",
        common::MODEL,
        "--no-confinement",
    ])
    .unwrap()
}

fn write(id: &'static str, path: &str, content: &str) -> Step {
    Step::Tool {
        id,
        name: "write_file",
        args: json!({"path": path, "content": content}),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tier_one_call_runs_without_a_prompt() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("notes.txt"), "TIER-ONE-CONTENT\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "r1",
            name: "read_file",
            args: json!({"path": "notes.txt"}),
        },
        Step::Text("read it"),
    ]);
    let setup = Setup::default();
    let daemon = Running::start(&ws.root, &llm, &setup).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("read notes.txt".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "read it");
    assert!(result_text(&llm, 1, "r1").contains("TIER-ONE-CONTENT"));
    assert_eq!(client.escalations(), 0);
    assert!(
        setup.classifier.seen.lock().is_empty(),
        "no review for a read"
    );
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reviewed_call_shows_the_dialog_and_runs_after_approve() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        write("w1", "a.txt", "APPROVED"),
        Step::Text("written"),
    ]);
    let daemon = Running::start(&ws.root, &llm, &Setup::default()).await;
    let mut client = Client::attach(&ws.root).await;
    client.set_mode(PermissionMode::Manual).await;
    client.send(Effect::StartTurn("write a.txt".into()));
    let (prompt, id) = client.escalation().await;
    assert_eq!(prompt.tool, "write_file");
    assert_eq!(prompt.rule.as_deref(), Some("write_file a.txt"));
    assert!(
        prompt
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("manual mode"),
        "{prompt:?}"
    );
    assert!(
        !ws.root.join("a.txt").exists(),
        "nothing runs before the answer"
    );
    client.send(Effect::RespondEscalation(id, EscalationAnswer::AllowOnce));
    client
        .wait_for(|msg| matches!(msg, Msg::EscalationResolved(closed) if *closed == id))
        .await;
    assert_eq!(client.turn_finished().await.result.unwrap().text, "written");
    assert_eq!(
        std::fs::read_to_string(ws.root.join("a.txt")).unwrap(),
        "APPROVED"
    );
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deny_with_a_comment_refuses_and_the_comment_reaches_the_model() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![write("w1", "a.txt", "NO"), Step::Text("ok")]);
    let daemon = Running::start(&ws.root, &llm, &Setup::default()).await;
    let mut client = Client::attach(&ws.root).await;
    client.set_mode(PermissionMode::Manual).await;
    client.send(Effect::StartTurn("write a.txt".into()));
    let (_, id) = client.escalation().await;
    client.send(Effect::RespondEscalation(
        id,
        EscalationAnswer::Deny {
            comment: Some("write b.txt instead".into()),
        },
    ));
    client.turn_finished().await.result.unwrap();
    assert!(!ws.root.join("a.txt").exists());
    let text = result_text(&llm, 1, "w1");
    assert!(
        text.contains("denied by the user: write b.txt instead"),
        "{text}"
    );
    assert!(text.contains("Do what the user says instead"), "{text}");
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allow_and_add_rule_skips_the_dialog_the_second_time() {
    let ws = Workspace::new();
    let marker = ws.root.join("marker");
    let command: &'static str = Box::leak(format!("touch {}", marker.display()).into_boxed_str());
    let shell = |id: &'static str| Step::Tool {
        id,
        name: "shell",
        args: json!({ "command": command }),
    };
    let llm = ScriptedLlmClient::new(vec![
        shell("s1"),
        Step::Text("first"),
        shell("s2"),
        Step::Text("second"),
    ]);
    let daemon = Running::start(&ws.root, &llm, &Setup::default()).await;
    let mut client = Client::attach(&ws.root).await;
    client.set_mode(PermissionMode::Manual).await;
    client.send(Effect::StartTurn("touch the marker".into()));
    let (prompt, id) = client.escalation().await;
    assert_eq!(
        prompt.rule.as_deref(),
        Some(format!("shell {command}").as_str())
    );
    client.send(Effect::RespondEscalation(
        id,
        EscalationAnswer::AllowAndAddRule,
    ));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "first");
    assert!(marker.exists());
    std::fs::remove_file(&marker).unwrap();
    client.send(Effect::StartTurn("again".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "second");
    assert_eq!(client.escalations(), 1, "the rule answered the second call");
    assert!(marker.exists());
    client.send(Effect::ListRules);
    client
        .wait_for(
            |msg| matches!(msg, Msg::Notice { text, .. } if text.contains("allow shell touch")),
        )
        .await;
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unanswered_request_times_out_denied_and_the_ui_recovers() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        write("w1", "a.txt", "LATE"),
        Step::Text("gave up"),
        Step::Text("next"),
    ]);
    let setup = Setup {
        escalation_timeout: Some(Duration::from_secs(1)),
        ..Setup::default()
    };
    let daemon = Running::start(&ws.root, &llm, &setup).await;
    let mut client = Client::attach(&ws.root).await;
    client.set_mode(PermissionMode::Manual).await;
    client.send(Effect::StartTurn("write a.txt".into()));
    let (_, id) = client.escalation().await;
    client
        .wait_for(|msg| matches!(msg, Msg::EscalationResolved(closed) if *closed == id))
        .await;
    assert_eq!(client.turn_finished().await.result.unwrap().text, "gave up");
    assert!(!ws.root.join("a.txt").exists());
    let text = result_text(&llm, 1, "w1");
    assert!(
        text.contains("did not answer the approval request in time"),
        "{text}"
    );
    // A late answer is a no-op; the UI serves the next prompt.
    client.send(Effect::RespondEscalation(id, EscalationAnswer::AllowOnce));
    client.send(Effect::StartTurn("next".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "next");
    assert!(!ws.root.join("a.txt").exists());
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_see_the_same_request_and_the_first_answer_wins() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![write("w1", "a.txt", "FIRST"), Step::Text("done")]);
    let daemon = Running::start(&ws.root, &llm, &Setup::default()).await;
    let mut first = Client::attach(&ws.root).await;
    let mut second = Client::attach(&ws.root).await;
    first.set_mode(PermissionMode::Manual).await;
    first.send(Effect::StartTurn("write a.txt".into()));
    let (prompt_a, id_a) = first.escalation().await;
    let (prompt_b, id_b) = second.escalation().await;
    assert_eq!(prompt_a, prompt_b);
    second.send(Effect::RespondEscalation(id_b, EscalationAnswer::AllowOnce));
    // The first client's dialog closes, saying why.
    first
        .wait_for(|msg| matches!(msg, Msg::EscalationResolved(closed) if *closed == id_a))
        .await;
    first
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("another client")))
        .await;
    // Its late answer changes nothing.
    first.send(Effect::RespondEscalation(
        id_a,
        EscalationAnswer::Deny { comment: None },
    ));
    assert_eq!(first.turn_finished().await.result.unwrap().text, "done");
    assert_eq!(
        std::fs::read_to_string(ws.root.join("a.txt")).unwrap(),
        "FIRST"
    );
    first.quit().await;
    second.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_mode_needs_no_dialog_and_plan_mode_refuses_writes() {
    let ws = Workspace::new();
    let marker = ws.root.join("made-by-shell");
    let command: &'static str = Box::leak(format!("touch {}", marker.display()).into_boxed_str());
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "s1",
            name: "shell",
            args: json!({ "command": command }),
        },
        Step::Text("auto done"),
        write("w1", "plan.txt", "NO"),
        Step::Text("planned"),
    ]);
    let setup = Setup::default();
    let daemon = Running::start(&ws.root, &llm, &setup).await;
    let mut client = Client::attach(&ws.root).await;
    client.set_mode(PermissionMode::Auto).await;
    client.send(Effect::StartTurn("touch the marker".into()));
    assert_eq!(
        client.turn_finished().await.result.unwrap().text,
        "auto done"
    );
    assert!(marker.exists(), "the classifier allowed it");
    assert_eq!(setup.classifier.seen.lock().len(), 1);
    // The classifier read the user's words, recorded by the daemon.
    assert!(setup.classifier.seen.lock()[0].contains("touch the marker"));
    client.set_mode(PermissionMode::Plan).await;
    client.send(Effect::StartTurn("plan a write".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "planned");
    assert!(!ws.root.join("plan.txt").exists());
    assert!(result_text(&llm, 3, "w1").contains("plan mode is on"));
    // The orchestrator was told plan mode started.
    let told = llm.requests()[2].iter().any(|m| match m {
        Message::System(_) | Message::SystemNotice(_) | Message::User(_) => {
            format!("{m:?}").contains("Plan mode is on")
        }
        _ => false,
    });
    assert!(told, "plan-mode note in the request");
    assert_eq!(client.escalations(), 0);
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deny_rule_added_in_attach_mode_refuses_the_next_call() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![write("w1", "a.txt", "NO"), Step::Text("refused")]);
    let daemon = Running::start(&ws.root, &llm, &Setup::default()).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::AddRule {
        effect: RuleEffect::Deny,
        tool: "write_file".into(),
        pattern: None,
    });
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text == "rule added"))
        .await;
    client.send(Effect::StartTurn("write a.txt".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "refused");
    assert!(!ws.root.join("a.txt").exists());
    let text = result_text(&llm, 1, "w1");
    assert!(text.contains("deny rule"), "{text}");
    assert_eq!(client.escalations(), 0);
    // The rule lives in the daemon's engine (and its realm rules file).
    client.send(Effect::ListRules);
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("deny write_file (realm)")))
        .await;
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helpers_reviewed_call_raises_an_approval_too() {
    let ws = Workspace::new();
    // The orchestrator and its helper share the scripted client and may
    // call it in either order: each answer is chosen by what was asked.
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let results: Vec<&str> = messages
            .iter()
            .filter_map(|m| match m {
                Message::ToolResults { results, .. } => Some(
                    results
                        .iter()
                        .map(|r| r.tool_use_id.as_str())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect();
        let asked_helper = common::user_texts(messages)
            .iter()
            .any(|text| text.contains("HELPER-TASK"));
        if results.contains(&"h1") {
            Step::Text("helper done")
        } else if results.contains(&"s1") {
            Step::Text("spawned")
        } else if asked_helper {
            write("h1", "helper.txt", "FROM-HELPER")
        } else {
            Step::Tool {
                id: "s1",
                name: "spawn_member",
                args: json!({
                    "profile": "main",
                    "member_id": "scout",
                    "initial_message": "HELPER-TASK: write helper.txt",
                }),
            }
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step.clone(), step.clone(), step.clone(), step]);
    let daemon = Running::start(&ws.root, &llm, &Setup::default()).await;
    let mut client = Client::attach(&ws.root).await;
    client.set_mode(PermissionMode::Manual).await;
    client.send(Effect::StartTurn("have a helper write helper.txt".into()));
    // Manual mode asks for the spawn as well; then the helper's write asks,
    // naming the helper.
    let (prompt, id) = client.escalation().await;
    assert_eq!(
        (prompt.tool.as_str(), prompt.agent.as_deref()),
        ("spawn_member", None)
    );
    client.send(Effect::RespondEscalation(id, EscalationAnswer::AllowOnce));
    let (prompt, id) = client.escalation().await;
    assert_eq!(prompt.tool, "write_file");
    assert_eq!(prompt.agent.as_deref(), Some("scout"), "{prompt:?}");
    client.send(Effect::RespondEscalation(id, EscalationAnswer::AllowOnce));
    within(async {
        while !ws.root.join("helper.txt").exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(ws.root.join("helper.txt")).unwrap(),
        "FROM-HELPER"
    );
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_daemon_keeps_the_application_policy_binding() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Text("seated"),
        write("w1", "a.txt", "NO"),
        Step::Text("refused after restart"),
    ]);
    let setup = Setup::default();
    let daemon = Running::start(&ws.root, &llm, &setup).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("hello".into()));
    client.turn_finished().await.result.unwrap();
    client.send(Effect::AddRule {
        effect: RuleEffect::Deny,
        tool: "write_file".into(),
        pattern: None,
    });
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text == "rule added"))
        .await;
    client.quit().await;
    daemon.stop().await;

    // A daemon without the registry: the member's durable binding names a
    // provider nothing serves.
    let bare = Setup {
        without_registry: true,
        ..setup.clone()
    };
    let refused = within(Running::try_start(&ws.root, &llm, &bare)).await;
    let refusal = match refused {
        Err(err) => Some(err.to_string()),
        Ok(daemon) => {
            // Started: the rebuild is lazy; the next turn shows the refusal.
            let running = Running::serve(daemon);
            let mut client = Client::attach(&ws.root).await;
            client.send(Effect::StartTurn("try".into()));
            let outcome = client.turn_finished().await;
            client.quit().await;
            running.stop().await;
            outcome.result.err().map(|failure| format!("{failure:?}"))
        }
    };
    // Observed: the daemon starts (the member is rebuilt lazily) and the
    // first delivery fails the member's restore: "a governed application
    // tool policy requires the host policy registry".
    let refusal = refusal.expect("a member bound to the policy is not rebuilt without it");
    assert!(
        refusal.contains("requires the host policy registry"),
        "{refusal}"
    );

    // With the registry the member rebuilds bound, and the deny rule (the
    // realm's) is enforced.
    let daemon = Running::start(&ws.root, &llm, &setup).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("write a.txt".into()));
    assert_eq!(
        client.turn_finished().await.result.unwrap().text,
        "refused after restart"
    );
    assert!(!ws.root.join("a.txt").exists());
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_local_methods_answer_before_the_engine() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm, &Setup::default()).await;
    let connection = daemon.connect().await;
    let status = connection.call("rkat/get_mode", json!({})).await.unwrap();
    assert_eq!(status["mode"], "auto", "{status}");
    assert_eq!(status["denials"], json!([0, 3, 0, 20]));
    let switched = connection
        .call("rkat/set_mode", json!({"mode": "accept-edits"}))
        .await
        .unwrap();
    assert_eq!(switched["status"]["mode"], "accept-edits", "{switched}");
    assert!(
        switched["notice"]
            .as_str()
            .unwrap()
            .contains("accept edits")
    );
    let err = connection
        .call("rkat/set_mode", json!({"mode": "yolo"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("-32602"), "{err}");
    let err = connection
        .call("rkat/nope", json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("-32601"), "{err}");
    let listed: Value = connection
        .call("rkat/rules", json!({"op": "list"}))
        .await
        .unwrap();
    assert_eq!(listed["ok"], true);
    let added = connection
        .call(
            "rkat/rules",
            json!({"op": "add", "deny": true, "tool": "shell", "pattern": "git push*"}),
        )
        .await
        .unwrap();
    assert_eq!(added["text"], "rule added");
    let listed = connection
        .call("rkat/rules", json!({"op": "list"}))
        .await
        .unwrap();
    assert!(
        listed["text"]
            .as_str()
            .unwrap()
            .contains("deny shell git push*"),
        "{listed}"
    );
    // The engine's own methods still answer on the same connection.
    let members = connection
        .call("mob/members", json!({"mob_id": "tui"}))
        .await
        .unwrap();
    assert!(members.to_string().contains("main"), "{members}");
    drop(connection);
    daemon.stop().await;
}

/// A helper's `shell` is the engine's native shell to the gate, as
/// in-process (where the gate reads the helper's own definitions): an allow
/// rule for it matches, and the classifier is not asked. The helper runs
/// from its spawn on, so its own catalog is never read before its first
/// call; the daemon judges it by the natives of the catalogs it has read.
#[cfg(feature = "mob")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helpers_shell_is_judged_as_the_native_shell() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "d1",
            name: "delegate",
            args: json!({
                "member_id": "scout",
                "task": "HELPER-TASK run it",
                "result_label": "result",
                "max_text_bytes": 4000,
            }),
        },
        Step::Tool {
            id: "sh",
            name: "shell",
            args: json!({"command": "echo HELPER-SHELL-RAN"}),
        },
        Step::Text("HELPER-DONE"),
        Step::Text("parent done"),
    ]);
    let setup = Setup::default();
    let daemon = Running::start(&ws.root, &llm, &setup).await;
    let connection = daemon.connect().await;
    let added = connection
        .call(
            "rkat/rules",
            json!({"op": "add", "deny": false, "tool": "shell", "pattern": "echo HELPER-SHELL*"}),
        )
        .await
        .unwrap();
    assert_eq!(added["text"], "rule added", "{added}");
    connection
        .call(
            "mob/turn_start",
            json!({"mob_id": "tui", "agent_identity": "main", "prompt": "delegate it"}),
        )
        .await
        .unwrap();
    let recorded = llm.recorded();
    let helper_after_shell = recorded
        .iter()
        // The command's output, after the result's exit-code line (the
        // call's arguments hold no newline before it).
        .find(|request| format!("{request:?}").contains("\\nHELPER-SHELL-RAN"))
        .map(|request| format!("{request:?}"));
    assert!(
        helper_after_shell.is_some(),
        "the helper's shell ran: {recorded:?}"
    );
    let asked = setup.classifier.tools.lock().clone();
    assert!(
        !asked.iter().any(|tool| tool == "shell"),
        "the allow rule decided the helper's shell, not the classifier: {asked:?}"
    );
    drop(connection);
    daemon.stop().await;
}

/// A daemon restarted in a persisted plan mode tells the orchestrator, and
/// leaving plan mode afterwards says so (its "told" state starts from the
/// persisted mode, not from "never told").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_mode_survives_a_restart_and_leaving_it_is_announced() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("in plan"), Step::Text("out of plan")]);
    let setup = Setup::default();
    let daemon = Running::start(&ws.root, &llm, &setup).await;
    let connection = daemon.connect().await;
    let set = connection
        .call("rkat/set_mode", json!({"mode": "plan"}))
        .await
        .unwrap();
    assert!(set.get("warning").is_none(), "{set}");
    drop(connection);
    daemon.stop().await;

    let daemon = Running::start(&ws.root, &llm, &setup).await;
    let connection = daemon.connect().await;
    let mode = connection.call("rkat/get_mode", json!({})).await.unwrap();
    assert_eq!(mode["mode"], "plan", "{mode}");
    let turn = |prompt: &str| json!({"mob_id": "tui", "agent_identity": "main", "prompt": prompt});
    connection
        .call("mob/turn_start", turn("first"))
        .await
        .unwrap();
    let request = format!("{:?}", llm.requests()[0]);
    assert!(request.contains("Plan mode is on"), "{request}");
    let set = connection
        .call("rkat/set_mode", json!({"mode": "default"}))
        .await
        .unwrap();
    assert!(set.get("warning").is_none(), "{set}");
    connection
        .call("mob/turn_start", turn("second"))
        .await
        .unwrap();
    let request = format!("{:?}", llm.requests()[1]);
    assert!(request.contains("Plan mode is off"), "{request}");
    drop(connection);
    daemon.stop().await;
}

/// `/trust` and `/rules trust` in attach mode act on the daemon's trust
/// record: a project hook file nobody trusted is refused (its hook does not
/// run), `/trust` from the client records it, and a restarted daemon keeps
/// it trusted (the hook runs on the rebuilt agent's next call).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trust_from_the_client_records_the_project_hooks_across_a_restart() {
    let ws = Workspace::new();
    std::fs::create_dir_all(ws.root.join(".rkat")).unwrap();
    std::fs::write(ws.root.join("notes.txt"), "NOTES\n").unwrap();
    let marker = ws.sibling("hook-entered");
    let hooks = format!(
        r#"[[hooks.entries]]
id = "probe"
point = "pre_tool_execution"
capability = "guardrail"
timeout_ms = 5000

[hooks.entries.runtime]
type = "command"
command = "/bin/sh"
args = ["-c", "printf entered > \"$1\"; cat >/dev/null; printf '{{}}'", "hook-probe", "{}"]
"#,
        marker.display()
    );
    std::fs::write(ws.root.join(".rkat/config.toml"), hooks).unwrap();
    let read = |id: &'static str| Step::Tool {
        id,
        name: "read_file",
        args: json!({"path": "notes.txt"}),
    };
    let llm = ScriptedLlmClient::new(vec![
        read("r1"),
        Step::Text("read, untrusted"),
        read("r2"),
        Step::Text("read, trusted"),
    ]);
    let setup = Setup {
        user_root_outside: true,
        ..Setup::default()
    };
    let daemon = Running::start(&ws.root, &llm, &setup).await;
    let connection = daemon.connect().await;
    let status = |listed: &Value| {
        listed["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| {
                file["path"]
                    .as_str()
                    .unwrap()
                    .ends_with(".rkat/config.toml")
            })
            .map(|file| file["status"].as_str().unwrap().to_string())
    };
    let listed = connection
        .call("rkat/trust", json!({"op": "list"}))
        .await
        .unwrap();
    assert_eq!(status(&listed).as_deref(), Some("untrusted"), "{listed}");
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("read notes.txt".into()));
    client.turn_finished().await.result.unwrap();
    assert!(!marker.exists(), "an untrusted project hook does not run");

    client.send(Effect::TrustProject);
    client
        .wait_for(|msg| {
            matches!(msg, Msg::Notice { text, .. } if text.starts_with("trusted ") && text.contains(".rkat/config.toml"))
        })
        .await;
    // `/rules trust` reaches the daemon too: the realm rules file a rule
    // created is summarised and trusted.
    client.send(Effect::AddRule {
        effect: RuleEffect::Allow,
        tool: "grep".into(),
        pattern: None,
    });
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text == "rule added"))
        .await;
    client.send(Effect::TrustRealmRules);
    client
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("trusted; 1 allow rule(s) apply now")))
        .await;
    client.quit().await;
    drop(connection);
    daemon.stop().await;

    let daemon = Running::start(&ws.root, &llm, &setup).await;
    let connection = daemon.connect().await;
    let listed = connection
        .call("rkat/trust", json!({"op": "list"}))
        .await
        .unwrap();
    assert_eq!(status(&listed).as_deref(), Some("trusted"), "{listed}");
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("read notes.txt again".into()));
    client.turn_finished().await.result.unwrap();
    assert!(marker.exists(), "the trusted hook ran on the rebuilt agent");
    client.quit().await;
    drop(connection);
    daemon.stop().await;
}

/// A seated helper's own tool catalog is read once its first run has
/// ended (the session is idle then), and cached for its later calls, as
/// the orchestrator's is.
#[cfg(feature = "mob")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seated_helpers_catalog_is_read_after_its_first_run() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let results: Vec<String> = messages
            .iter()
            .filter_map(|m| match m {
                Message::ToolResults { results, .. } => Some(
                    results
                        .iter()
                        .map(|r| r.tool_use_id.clone())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect();
        let asked_helper = common::user_texts(messages)
            .iter()
            .any(|text| text.contains("HELPER-TASK"));
        if asked_helper {
            Step::Text("helper done")
        } else if results.iter().any(|id| id == "s1") {
            Step::Text("spawned")
        } else {
            Step::Tool {
                id: "s1",
                name: "spawn_member",
                args: json!({
                    "profile": "main",
                    "member_id": "scout",
                    "initial_message": "HELPER-TASK: say done",
                }),
            }
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step.clone(), step.clone(), step]);
    let daemon = Running::start(&ws.root, &llm, &Setup::default()).await;
    let connection = daemon.connect().await;
    connection
        .call(
            "mob/turn_start",
            json!({"mob_id": "tui", "agent_identity": "main", "prompt": "seat a scout"}),
        )
        .await
        .unwrap();
    let helper = within(async {
        loop {
            let status = connection
                .call(
                    "mob/member_status",
                    json!({"mob_id": "tui", "agent_identity": "scout"}),
                )
                .await;
            if let Ok(status) = status
                && let Some(session) = status["current_session_id"].as_str()
            {
                return meerkat_core::SessionId::parse(session).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    within(async {
        while !daemon.origins.has(&helper) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    // Its own catalog: the native shell as the engine defines it.
    assert!(daemon.origins.origin(&helper, "shell").is_some());
    drop(connection);
    daemon.stop().await;
}
