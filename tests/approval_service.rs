//! The approval gate end to end, through the `rkat-tui-mob` daemon with a
//! scripted LLM and an attached client: every tool call of the
//! agent passes the gate the daemon installed as its
//! `ToolDispatchAdmission`. A Tier-1 read runs; a classifier block reaches
//! the model as a reason and the run continues; a classifier allow runs the
//! call; an escalation is answered through the client's effect channel; a
//! conversation the daemon restores has its classifier transcript seeded
//! from its history.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/rig.rs"]
mod rig;

use std::sync::Arc;
use std::time::Duration;

use common::{ScriptedLlmClient, Step, Workspace, within};
use meerkat_core::types::ToolResult;
use meerkat_core::{AgentEvent, Message};
use rig::{Attached, ScriptedClassifier, Setup};
use rkat_tui::approval::classifier::{ActionClassifier, ClassifierError, ClassifierVerdict};
use rkat_tui::ui::{ApprovalNotice, Effect, EscalationAnswer, Msg};
use serde_json::json;

fn setup(classifier: &Arc<ScriptedClassifier>) -> Setup {
    Setup::default().classifier(Arc::clone(classifier) as Arc<dyn ActionClassifier>)
}

/// The result of tool call `id` in `messages`.
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

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tier_one_read_runs_without_review() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("notes.txt"), "READ-ME-CONTENT\n").unwrap();
    let classifier = ScriptedClassifier::new(vec![]);
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "r1",
            name: "read_file",
            args: json!({"path": "notes.txt"}),
        },
        Step::Text("read it"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup(&classifier)).await;
    rig.send(Effect::StartTurn("read notes.txt".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.expect("turn succeeds").text, "read it");
    let result = tool_result(&llm.requests()[1], "r1");
    assert!(!result.is_error, "{}", result.text_content());
    assert!(result.text_content().contains("READ-ME-CONTENT"));
    assert!(classifier.seen().is_empty(), "tier 1 needs no classifier");
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_classifier_block_reaches_the_model_and_the_run_continues() {
    let ws = Workspace::new();
    let marker = ws.sibling("pwned");
    let command = leak(format!("touch {}", marker.display()));
    let classifier = ScriptedClassifier::new(vec![Ok(ClassifierVerdict::Block {
        rule: Some("D1".into()),
        reason: "touches files outside the project".into(),
        annotation: Some("[classifier checked 0 files]".into()),
    })]);
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "s1",
            name: "shell",
            args: json!({ "command": command }),
        },
        Step::Text("understood, I will not do that"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup(&classifier)).await;
    rig.send(Effect::StartTurn("USER-ASK tidy up".into()));

    let notice = rig
        .wait_for(|msg| matches!(msg, Msg::Approval(ApprovalNotice { allowed: false, .. })))
        .await;
    let Msg::Approval(notice) = notice else {
        unreachable!()
    };
    assert_eq!(notice.tool, "shell");
    assert!(
        notice
            .reason
            .as_deref()
            .is_some_and(|r| r
                .contains("touches files outside the project [D1] [classifier checked 0 files]")),
        "{notice:?}"
    );

    let outcome = rig.turn_finished().await;
    assert_eq!(
        outcome.result.expect("the run continues").text,
        "understood, I will not do that"
    );
    assert!(!marker.exists(), "the denied command never ran");
    // The UI's tool block for the call ends as an error.
    assert!(rig.events().iter().any(|event| matches!(
        event,
        AgentEvent::ToolExecutionCompleted { id, is_error: true, .. } if id == "s1"
    )));
    let result = tool_result(&llm.requests()[1], "s1");
    assert!(result.is_error);
    let text = result.text_content();
    assert!(text.contains("policy_denied"), "{text}");
    assert!(
        text.contains("Blocked by auto-mode safety policy: touches files outside the project."),
        "{text}"
    );
    // The rule tag and how the classifier checked are the user's: the
    // notice shows them (above), the model's text carries neither.
    assert!(
        !text.contains("[D1]") && !text.contains("classifier checked"),
        "{text}"
    );
    // The classifier saw the user's words and the call, nothing else.
    let seen = classifier.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "shell");
    assert!(seen[0].2.contains("USER-ASK tidy up"), "{}", seen[0].2);
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_classifier_allow_runs_the_call() {
    let ws = Workspace::new();
    let outside = ws.sibling("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join("allowed.txt");
    let path = leak(target.display().to_string());
    let classifier = ScriptedClassifier::new(vec![Ok(ClassifierVerdict::allow("ok"))]);
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "w1",
            name: "write_file",
            args: json!({ "path": path, "content": "ALLOWED\n" }),
        },
        Step::Text("written"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup(&classifier)).await;
    rig.send(Effect::StartTurn("write the file outside".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.expect("turn succeeds").text, "written");
    assert_eq!(
        classifier.seen().len(),
        1,
        "outside the project needs review"
    );
    let result = tool_result(&llm.requests()[1], "w1");
    assert!(!result.is_error, "{}", result.text_content());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "ALLOWED\n");
    rig.quit().await;
}

/// rkat-tui's own file tools cannot write the project's `.rkat/` inputs
/// (`mcp.toml`, `tui.toml`, `config.toml` with its hooks, a hook script):
/// tier 0 refuses each call before any rule or the classifier, which
/// allows everything here, and nothing lands on disk (the engine's
/// in-process file tools could, lukacf/meerkat#2170; the agent gets ours).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_file_tools_cannot_write_the_projects_rkat_inputs() {
    let ws = Workspace::new();
    let targets = [
        ".rkat/mcp.toml",
        ".rkat/tui.toml",
        ".rkat/config.toml",
        ".rkat/hooks/pre-turn.sh",
    ];
    let classifier = ScriptedClassifier::allowing(Vec::new());
    let calls: Vec<(&'static str, &'static str, serde_json::Value)> = targets
        .iter()
        .zip(["w0", "w1", "w2", "w3"])
        .map(|(path, id)| (id, "write_file", json!({"path": path, "content": "x\n"})))
        .collect();
    let llm = ScriptedLlmClient::new(vec![Step::Tools(calls), Step::Text("tried")]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup(&classifier)).await;
    rig.send(Effect::StartTurn("configure yourself".into()));
    assert_eq!(rig.turn_finished().await.result.unwrap().text, "tried");
    let request = &llm.requests()[1];
    for (path, id) in targets.iter().zip(["w0", "w1", "w2", "w3"]) {
        let result = tool_result(request, id);
        assert!(result.is_error, "{path}: {}", result.text_content());
        assert!(!ws.root.join(path).exists(), "{path} was written");
    }
    assert!(classifier.seen().is_empty(), "sealed before the classifier");
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_escalation_is_answered_through_the_runner() {
    let ws = Workspace::new();
    let outside = ws.sibling("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join("escalated.txt");
    let path = leak(target.display().to_string());
    // The classifier fails: the call goes to the human.
    let classifier = ScriptedClassifier::new(vec![Err(ClassifierError::Provider("down".into()))]);
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "w1",
            name: "write_file",
            args: json!({ "path": path, "content": "HUMAN-OK\n" }),
        },
        Step::Text("done"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup(&classifier)).await;
    rig.send(Effect::StartTurn("write it".into()));
    let msg = rig.wait_for(|msg| matches!(msg, Msg::Escalation(..))).await;
    let Msg::Escalation(prompt, id) = msg else {
        unreachable!()
    };
    assert_eq!(prompt.tool, "write_file");
    assert!(
        prompt
            .classifier_hint
            .as_deref()
            .is_some_and(|h| h.contains("down"))
    );
    rig.send(Effect::RespondEscalation(id, EscalationAnswer::AllowOnce));
    rig.wait_for(|msg| matches!(msg, Msg::EscalationResolved(resolved) if *resolved == id))
        .await;
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.expect("turn succeeds").text, "done");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "HUMAN-OK\n");

    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unanswered_escalation_times_out_as_a_denial() {
    let ws = Workspace::new();
    let marker = ws.sibling("never");
    let path = leak(marker.display().to_string());
    let classifier = ScriptedClassifier::new(vec![Err(ClassifierError::Provider("down".into()))]);
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "w1",
            name: "write_file",
            args: json!({ "path": path, "content": "x" }),
        },
        Step::Text("gave up"),
    ]);
    let mut setup = setup(&classifier);
    setup.escalation_timeout = Some(Duration::from_millis(200));
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    rig.send(Effect::StartTurn("write it".into()));
    // Nobody answers: the dialog closes and the model is told why. On a
    // slow host the turn may end before the dialog shows (the request
    // timed out first), so each is looked for among what was seen too.
    rig.saw(|msg| matches!(msg, Msg::Escalation(..))).await;
    let id = rig
        .seen
        .iter()
        .find_map(|msg| match msg {
            Msg::Escalation(_, id) => Some(*id),
            _ => None,
        })
        .unwrap();
    rig.saw(|msg| matches!(msg, Msg::EscalationResolved(resolved) if *resolved == id))
        .await;
    rig.saw(|msg| matches!(msg, Msg::TurnFinished(_))).await;
    let outcome = rig
        .seen
        .iter()
        .find_map(|msg| match msg {
            Msg::TurnFinished(outcome) => Some(outcome.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(outcome.result.expect("the run continues").text, "gave up");
    assert!(!marker.exists());
    let text = tool_result(&llm.requests()[1], "w1").text_content();
    assert!(
        text.contains("did not answer the approval request in time"),
        "{text}"
    );
    rig.quit().await;
}

/// "No, and tell the agent what to do instead": the comment the user wrote
/// reaches the model in the denied call's tool error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deny_comment_reaches_the_model_as_the_tool_error() {
    let ws = Workspace::new();
    let classifier = ScriptedClassifier::new(vec![]);
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "w1",
            name: "write_file",
            args: json!({ "path": ".env.local", "content": "TOKEN=1\n" }),
        },
        Step::Text("understood"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup(&classifier)).await;
    rig.send(Effect::StartTurn("configure the token".into()));
    let Msg::Escalation(_, id) = rig.wait_for(|msg| matches!(msg, Msg::Escalation(..))).await
    else {
        unreachable!()
    };
    rig.send(Effect::RespondEscalation(
        id,
        EscalationAnswer::Deny {
            comment: Some("put it in .env.example instead".into()),
        },
    ));
    assert_eq!(
        rig.turn_finished().await.result.expect("turn").text,
        "understood"
    );
    assert!(!ws.root.join(".env.local").exists());
    let result = tool_result(&llm.requests()[1], "w1");
    assert!(result.is_error);
    let text = result.text_content();
    assert!(
        text.contains("denied by the user: put it in .env.example instead"),
        "{text}"
    );
    assert!(text.contains("Do what the user says instead"), "{text}");
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_session_reseeds_the_classifier_transcript() {
    let ws = Workspace::new();
    let first = ScriptedLlmClient::new(vec![Step::Text("noted")]);
    let mut rig = Attached::start(&ws.root, &first).await;
    rig.send(Effect::StartTurn("USER-EARLIER remember the plan".into()));
    rig.turn_finished().await;
    let id = rig.current_session().await;
    rig.quit().await;

    // A restarted daemon restores the conversation: its first reviewed
    // call carries the earlier user words, not the assistant's.
    let marker = ws.sibling("resumed");
    let command = leak(format!("touch {}", marker.display()));
    let classifier = ScriptedClassifier::new(vec![Ok(ClassifierVerdict::Block {
        rule: None,
        reason: "no".into(),
        annotation: None,
    })]);
    let second = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "s1",
            name: "shell",
            args: json!({ "command": command }),
        },
        Step::Text("ok"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &second, setup(&classifier)).await;
    assert_eq!(rig.current_session().await, id);
    rig.send(Effect::StartTurn("USER-NOW go on".into()));
    within(rig.turn_finished()).await;
    let seen = classifier.seen();
    assert_eq!(seen.len(), 1);
    let seeded = &seen[0].2;
    assert!(
        seeded.contains("USER-EARLIER remember the plan"),
        "{seeded}"
    );
    assert!(
        !seeded.contains("noted"),
        "assistant prose stays out: {seeded}"
    );
    assert!(seen[0].2.contains("USER-NOW go on"), "{}", seen[0].2);
    assert!(!marker.exists());
    rig.quit().await;
}

/// A protected write (`.env.local`) goes to the human, not the classifier
/// (scripted to allow): the dialog names the path and why; "deny" denies
/// it, "allow + rule" runs it and adds a rule for that file, and the next
/// identical write runs without a dialog.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_protected_write_asks_the_user_and_allow_always_stops_the_prompts() {
    let ws = Workspace::new();
    let target = ws.root.join(".env.local");
    let classifier = ScriptedClassifier::new(vec![
        Ok(ClassifierVerdict::allow("ok")),
        Ok(ClassifierVerdict::allow("ok")),
        Ok(ClassifierVerdict::allow("ok")),
    ]);
    let write = |content: &str| json!({ "path": ".env.local", "content": content });
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "w1",
            name: "write_file",
            args: write("DENIED=1\n"),
        },
        Step::Text("denied"),
        Step::Tool {
            id: "w2",
            name: "write_file",
            args: write("FIRST=1\n"),
        },
        Step::Text("first"),
        Step::Tool {
            id: "w3",
            name: "write_file",
            args: write("SECOND=1\n"),
        },
        Step::Text("second"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup(&classifier)).await;

    rig.send(Effect::StartTurn("set a secret".into()));
    let Msg::Escalation(prompt, id) = rig.wait_for(|msg| matches!(msg, Msg::Escalation(..))).await
    else {
        unreachable!()
    };
    assert_eq!(prompt.tool, "write_file");
    assert_eq!(prompt.classifier_hint, None, "no classifier opinion");
    assert_eq!(
        prompt.policy_reason.as_deref(),
        Some("protected path: `.env.local` (secrets); the policy always asks for these")
    );
    rig.send(Effect::RespondEscalation(
        id,
        EscalationAnswer::Deny { comment: None },
    ));
    let notice = rig
        .wait_for(|msg| matches!(msg, Msg::Approval(ApprovalNotice { allowed: false, .. })))
        .await;
    let Msg::Approval(notice) = notice else {
        unreachable!()
    };
    assert_eq!(notice.tier, "ask the user · human");
    assert_eq!(
        rig.turn_finished().await.result.expect("turn").text,
        "denied"
    );
    assert!(!target.exists());

    rig.send(Effect::StartTurn("set it after all".into()));
    let Msg::Escalation(_, id) = rig.wait_for(|msg| matches!(msg, Msg::Escalation(..))).await
    else {
        unreachable!()
    };
    rig.send(Effect::RespondEscalation(
        id,
        EscalationAnswer::AllowAndAddRule,
    ));
    assert_eq!(
        rig.turn_finished().await.result.expect("turn").text,
        "first"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "FIRST=1\n");
    let listed = rig
        .daemon
        .connect()
        .await
        .call("rkat/rules", json!({"op": "list"}))
        .await
        .unwrap();
    let listed = listed["text"].as_str().unwrap().to_string();
    let added: Vec<&str> = listed.lines().filter(|l| l.contains("allow ")).collect();
    assert_eq!(added.len(), 1, "{listed}");
    assert!(added[0].contains("allow write_file .env.local"), "{listed}");

    rig.send(Effect::StartTurn("change it".into()));
    assert_eq!(
        rig.turn_finished().await.result.expect("turn").text,
        "second"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "SECOND=1\n");
    let escalations = rig
        .seen
        .iter()
        .filter(|msg| matches!(msg, Msg::Escalation(..)))
        .count();
    assert_eq!(escalations, 2, "the rule stopped the prompts");
    assert!(
        classifier.seen().is_empty(),
        "the classifier was never asked"
    );
    rig.quit().await;
}
