//! Approvals in attach mode (P3): the daemon's orchestrator and its
//! helpers run behind the TUI's approval engine, the human answers in the
//! attached TUI over the engine's `approval/*` methods, the permission mode
//! and the rules reach the daemon through its `rkat/*` methods, and the
//! deny rules are the members' durable application tool policy. The daemon
//! runs in-process with a scripted LLM and a scripted classifier; the
//! clients are attach runners driven through the UI's channels.

#![cfg(feature = "mob")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::{ScriptedLlmClient, Step, Workspace, args_for, endpoint_file_for, no_lists, within};
use meerkat::LlmClient;
use meerkat_core::Message;
use meerkat_core::types::ToolResult;
use parking_lot::Mutex;
use rig::{Client, Running};
use rkat_tui::approval::classifier::{
    ActionClassifier, ClassifierError, ClassifierInput, ClassifierVerdict,
};
use rkat_tui::approval::mode::PermissionMode;
use rkat_tui::bootstrap::{ApprovalOverrides, BootstrapOptions, HostPaths};
use rkat_tui::daemon::{Daemon, DaemonError, DaemonOptions};
use rkat_tui::ui::{Effect, EscalationAnswer, Msg, RuleEffect, StatusUpdate};
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

/// Start a daemon with `setup` and serve it in the background.
async fn start(root: &Path, llm: &Arc<ScriptedLlmClient>, setup: &Setup) -> Running {
    let daemon = within(try_start(root, llm, setup))
        .await
        .expect("daemon starts");
    Running::serve(daemon)
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

/// Attach a client and wait for the daemon's mode in its status bar.
async fn attach(root: &Path) -> Client {
    let mut client = Client::attach(root).await;
    client
        .saw(|msg| matches!(msg, Msg::Status(StatusUpdate::Mode { .. })))
        .await;
    client
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
    let daemon = start(&ws.root, &llm, &setup).await;
    let mut client = attach(&ws.root).await;
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
    let daemon = start(&ws.root, &llm, &Setup::default()).await;
    let mut client = attach(&ws.root).await;
    client.set_mode(PermissionMode::Manual).await;
    client.send(Effect::StartTurn("write a.txt".into()));
    let (prompt, id) = client.escalation().await;
    assert_eq!(prompt.tool, "write_file");
    assert_eq!(prompt.rule().as_deref(), Some("write_file a.txt"));
    assert!(
        prompt
            .policy_reason
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
    // The decision is the record: the realm's approval store holds none.
    let listed = daemon
        .connect()
        .await
        .call("approval/list", json!({}))
        .await
        .unwrap();
    assert_eq!(listed["approvals"], json!([]), "{listed}");
    client.quit().await;
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deny_with_a_comment_refuses_and_the_comment_reaches_the_model() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![write("w1", "a.txt", "NO"), Step::Text("ok")]);
    let daemon = start(&ws.root, &llm, &Setup::default()).await;
    let mut client = attach(&ws.root).await;
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
    let daemon = start(&ws.root, &llm, &Setup::default()).await;
    let mut client = attach(&ws.root).await;
    client.set_mode(PermissionMode::Manual).await;
    client.send(Effect::StartTurn("touch the marker".into()));
    let (prompt, id) = client.escalation().await;
    assert_eq!(
        prompt.rule().as_deref(),
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
    let daemon = start(&ws.root, &llm, &setup).await;
    let mut client = attach(&ws.root).await;
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
    let daemon = start(&ws.root, &llm, &Setup::default()).await;
    let mut first = attach(&ws.root).await;
    let mut second = attach(&ws.root).await;
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
    let daemon = start(&ws.root, &llm, &setup).await;
    let mut client = attach(&ws.root).await;
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
    let daemon = start(&ws.root, &llm, &Setup::default()).await;
    let mut client = attach(&ws.root).await;
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
    let daemon = start(&ws.root, &llm, &Setup::default()).await;
    let mut client = attach(&ws.root).await;
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
    let daemon = start(&ws.root, &llm, &setup).await;
    let mut client = attach(&ws.root).await;
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
    let refused = within(try_start(&ws.root, &llm, &bare)).await;
    let refusal = match refused {
        Err(err) => Some(err.to_string()),
        Ok(daemon) => {
            // Started: the rebuild is lazy; the next turn shows the refusal.
            let running = Running::serve(daemon);
            let mut client = attach(&ws.root).await;
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
    let daemon = start(&ws.root, &llm, &setup).await;
    let mut client = attach(&ws.root).await;
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
    let daemon = start(&ws.root, &llm, &Setup::default()).await;
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
    let daemon = start(&ws.root, &llm, &setup).await;
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
    let daemon = start(&ws.root, &llm, &setup).await;
    let connection = daemon.connect().await;
    let set = connection
        .call("rkat/set_mode", json!({"mode": "plan"}))
        .await
        .unwrap();
    assert!(set.get("warning").is_none(), "{set}");
    drop(connection);
    daemon.stop().await;

    let daemon = start(&ws.root, &llm, &setup).await;
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
    let daemon = start(&ws.root, &llm, &setup).await;
    let connection = daemon.connect().await;
    // `rkat/trust` names its op; there is no list form (as in-process).
    for params in [json!({}), json!({"op": "list"})] {
        let refused = connection.call("rkat/trust", params.clone()).await;
        assert!(
            matches!(&refused, Err(rkat_tui::client::ClientError::Rpc { code, .. }) if *code == rkat_tui::daemon::protocol::INVALID_PARAMS),
            "{params}: {refused:?}"
        );
    }
    let mut client = attach(&ws.root).await;
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

    let daemon = start(&ws.root, &llm, &setup).await;
    let connection = daemon.connect().await;
    let mut client = attach(&ws.root).await;
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
    let daemon = start(&ws.root, &llm, &Setup::default()).await;
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

/// Overlapping mode changes (a client's timed-out `plan` and the user's
/// switch back, here many at once on two connections) leave the
/// orchestrator's last plan-mode note matching the mode that holds: the
/// check and the note are one step under the daemon's lock, and the note
/// follows the current mode, not the caller's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_mode_changes_leave_the_plan_note_matching_the_mode() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let daemon = start(&ws.root, &llm, &Setup::default()).await;
    let a = daemon.connect().await;
    let b = daemon.connect().await;
    let calls = (0..12).map(|i| {
        let connection = if i % 2 == 0 { &a } else { &b };
        let mode = if i % 3 == 0 { "default" } else { "plan" };
        connection.call("rkat/set_mode", json!({"mode": mode}))
    });
    for answer in futures::future::join_all(calls).await {
        let answer = answer.unwrap();
        assert!(answer.get("warning").is_none(), "{answer}");
    }
    let mode = a.call("rkat/get_mode", json!({})).await.unwrap();
    let plan = mode["mode"] == "plan";
    a.call(
        "mob/turn_start",
        json!({"mob_id": "tui", "agent_identity": "main", "prompt": "go"}),
    )
    .await
    .unwrap();
    let request = format!("{:?}", llm.requests()[0]);
    let on = request.rfind("Plan mode is on");
    let off = request.rfind("Plan mode is off");
    if plan {
        assert!(on.is_some() && on > off, "{mode}: {request}");
    } else {
        assert!(on.is_none() || off > on, "{mode}: {request}");
    }
    drop((a, b));
    daemon.stop().await;
}

/// The next notification `method` on a raw connection.
async fn next_note(
    notes: &mut tokio::sync::mpsc::UnboundedReceiver<rkat_tui::client::transport::Notification>,
    method: &str,
) -> Value {
    within(async {
        loop {
            let note = notes.recv().await.expect("connection open");
            if note.method == method {
                return note.params;
            }
        }
    })
    .await
}

/// P3b: a reviewed call is held as a durable decision the daemon pushes to
/// every attached connection (no polling); its `initialize` says so; a
/// decide naming who decided is refused; the first answer wins and a later
/// one gets the engine's own refusal; the clients shown it hear it ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reviewed_call_is_pushed_as_a_decision_and_the_first_answer_wins() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![write("w1", "a.txt", "PUSHED"), Step::Text("done")]);
    let daemon = start(&ws.root, &llm, &Setup::default()).await;
    let mut client = attach(&ws.root).await;
    let (raw, mut notes) = daemon.connect_with_notes().await;
    let caps = raw.call("initialize", json!({})).await.unwrap();
    let methods: Vec<&str> = caps["methods"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(methods.contains(&"decision/notice"), "{methods:?}");
    assert!(methods.contains(&"decision/decide"), "{methods:?}");
    client.set_mode(PermissionMode::Manual).await;
    client.send(Effect::StartTurn("write a.txt".into()));
    let notice = next_note(&mut notes, "decision/notice").await;
    assert_eq!(notice["kind"], "rkat-tui.tool-call");
    assert_eq!(notice["phase"], "open");
    let decision = notice["decision_id"].as_str().unwrap().to_string();
    let digest = notice["presentation"]["subject_digest"]
        .as_str()
        .unwrap()
        .to_string();
    let rendering = notice["presentation"]["rendering"].as_str().unwrap();
    assert!(rendering.contains("write_file"), "{rendering}");
    // The client was pushed the same request.
    let (_, id) = client.escalation().await;
    // Naming who decided is refused before the engine sees it.
    for field in ["actor", "approver"] {
        let mut params = json!({
            "decision_id": decision,
            "approve": true,
            "presented_digest": digest,
        });
        params[field] = json!("mallory");
        let refused = raw.call("decision/decide", params).await.unwrap_err();
        assert!(refused.to_string().contains(field), "{field}: {refused}");
    }
    client.send(Effect::RespondEscalation(id, EscalationAnswer::AllowOnce));
    let ended = next_note(&mut notes, "rkat/decision_ended").await;
    assert_eq!(ended["decision_id"], decision.as_str());
    assert_eq!(ended["phase"], "approved");
    let late = raw
        .call(
            "decision/decide",
            json!({"decision_id": decision, "approve": false, "presented_digest": digest}),
        )
        .await
        .unwrap_err();
    assert!(late.to_string().contains("not_open"), "{late}");
    assert_eq!(client.turn_finished().await.result.unwrap().text, "done");
    assert_eq!(
        std::fs::read_to_string(ws.root.join("a.txt")).unwrap(),
        "PUSHED"
    );
    let got = raw
        .call("decision/get", json!({"decision_id": decision}))
        .await
        .unwrap();
    assert_eq!(got["decision"]["phase"], "approved");
    assert_eq!(
        got["decision"]["outcome"]["evidence"]["approver_id"],
        got["decision"]["approver_id"]
    );
    drop(raw);
    client.quit().await;
    daemon.stop().await;
}

/// A client that attaches after the request opened is shown it (the
/// daemon's catch-up), and its answer counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_attaching_after_the_request_opened_is_shown_it() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![write("w1", "a.txt", "LATE"), Step::Text("done")]);
    let daemon = start(&ws.root, &llm, &Setup::default()).await;
    let mut first = attach(&ws.root).await;
    first.set_mode(PermissionMode::Manual).await;
    first.send(Effect::StartTurn("write a.txt".into()));
    let (prompt_a, id_a) = first.escalation().await;
    let mut late = attach(&ws.root).await;
    // Offered as the client attached (it may have arrived during attach).
    let shown = late.seen.iter().find_map(|msg| match msg {
        Msg::Escalation(prompt, id) => Some((prompt.clone(), *id)),
        _ => None,
    });
    let (prompt_b, id_b) = match shown {
        Some(shown) => shown,
        None => late.escalation().await,
    };
    assert_eq!(prompt_a, prompt_b);
    late.send(Effect::RespondEscalation(id_b, EscalationAnswer::AllowOnce));
    first
        .wait_for(|msg| matches!(msg, Msg::EscalationResolved(closed) if *closed == id_a))
        .await;
    first
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("another client")))
        .await;
    assert_eq!(first.turn_finished().await.result.unwrap().text, "done");
    assert_eq!(
        std::fs::read_to_string(ws.root.join("a.txt")).unwrap(),
        "LATE"
    );
    first.quit().await;
    late.quit().await;
    daemon.stop().await;
}

/// A decision a stopped daemon left open held a dispatch that ended with
/// it: the restarted daemon withdraws it, so no client is shown a request
/// whose answer would run nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_daemon_withdraws_the_request_its_predecessor_left_open() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        write("w1", "a.txt", "NEVER"),
        Step::Text("done"),
        Step::Text("done"),
    ]);
    let setup = Setup::default();
    let daemon = start(&ws.root, &llm, &setup).await;
    let mut client = attach(&ws.root).await;
    let (raw, mut notes) = daemon.connect_with_notes().await;
    client.set_mode(PermissionMode::Manual).await;
    client.send(Effect::StartTurn("write a.txt".into()));
    let notice = next_note(&mut notes, "decision/notice").await;
    let decision = notice["decision_id"].as_str().unwrap().to_string();
    client.escalation().await;
    drop(raw);
    client.quit().await;
    daemon.stop().await;
    let daemon = start(&ws.root, &llm, &setup).await;
    let (raw, mut notes) = daemon.connect_with_notes().await;
    let got = raw
        .call("decision/get", json!({"decision_id": decision}))
        .await
        .unwrap();
    assert_eq!(got["decision"]["phase"], "withdrawn", "{got}");
    // The old request is never offered again; the restored run may ask
    // again for its call, under a decision of its own.
    if let Ok(notice) = tokio::time::timeout(
        Duration::from_secs(1),
        next_note(&mut notes, "decision/notice"),
    )
    .await
    {
        assert_ne!(notice["decision_id"], decision.as_str(), "{notice}");
    }
    assert!(!ws.root.join("a.txt").exists());
    drop(raw);
    daemon.stop().await;
}

/// The engine's `fork_off` seats its child under the source's dispatch
/// admission (lukacf/meerkat#2112): an orchestrator's fork child is a
/// helper of the orchestrator's session behind the same gate, so its
/// write asks the human (naming the child) and a denial reaches it. Before
/// the daemon's members kept `fork_off`, the call was refused and no child
/// ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_off_child_runs_behind_the_gate() {
    const TASK: &str = "FORK-CHILD-TASK write forked.txt";
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let child = messages
            .iter()
            .any(|m| matches!(m, Message::User(_)) && format!("{m:?}").contains("FORK-CHILD-TASK"));
        let answered = matches!(messages.last(), Some(Message::ToolResults { .. }));
        match (child, answered) {
            (true, false) => Step::Tool {
                id: "fw1",
                name: "write_file",
                args: json!({"path": "forked.txt", "content": "FROM-FORK"}),
            },
            (true, true) => Step::Text("child done"),
            (false, false) => Step::Tool {
                id: "fo1",
                name: "fork_off",
                args: json!({"member_id": "forky", "task": TASK}),
            },
            (false, true) => Step::Text("parent done"),
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step.clone(), step.clone(), step.clone(), step]);
    let daemon = start(&ws.root, &llm, &Setup::default()).await;
    let mut client = attach(&ws.root).await;
    client.set_mode(PermissionMode::Manual).await;
    client.send(Effect::StartTurn("fork a child".into()));
    // `fork_off` itself runs in manual mode (the child is the helper the
    // gate asks about); the child's write asks, naming the child.
    let (prompt, id) = client.escalation().await;
    assert_eq!(prompt.tool, "write_file", "{prompt:?}");
    assert_eq!(prompt.agent.as_deref(), Some("forky"), "{prompt:?}");
    client.send(Effect::RespondEscalation(
        id,
        EscalationAnswer::Deny {
            comment: Some("not from a fork".into()),
        },
    ));
    let denied = within(async {
        loop {
            let seen = llm.requests().iter().find_map(|messages| {
                messages.iter().find_map(|m| match m {
                    Message::ToolResults { results, .. } => results
                        .iter()
                        .find(|r| r.tool_use_id == "fw1")
                        .map(ToolResult::text_content),
                    _ => None,
                })
            });
            if let Some(text) = seen {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(denied.contains("not from a fork"), "{denied}");
    assert!(!ws.root.join("forked.txt").exists());
    client.quit().await;
    daemon.stop().await;
}

/// A parameter refusal answers the request that caused it (upstream
/// #2114): the client's call fails at once with the engine's message
/// instead of waiting for an answer that never names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parameter_refusal_answers_its_request() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = start(&ws.root, &llm, &Setup::default()).await;
    let raw = daemon.connect().await;
    for (method, params) in [
        ("mob/stop_member_run", json!({"mob_id": "tui", "bogus": 1})),
        (
            "mob/stream_open",
            json!({"mob_id": "tui", "cursor": {"epoch": "x", "seq": 1}}),
        ),
        ("decision/get", json!({"decision_id": "not-a-decision"})),
    ] {
        let refused = tokio::time::timeout(Duration::from_secs(5), raw.call(method, params))
            .await
            .unwrap_or_else(|_| panic!("{method}: no answer named the request"));
        assert!(refused.is_err(), "{method}: {refused:?}");
    }
    drop(raw);
    daemon.stop().await;
}
