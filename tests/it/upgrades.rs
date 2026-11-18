//! A conversation an earlier build began, seated by this one: the agent's
//! and a restored helper's instructions are brought up to this build's
//! once (the earlier build's sentences replaced, not kept beside the new
//! ones), a compaction keeps them, and a later start finds them current
//! and sends nothing. The earlier build is a daemon started
//! `with_stale_instructions` (each of this build's sections with a
//! sentence of its own under the same heading).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::path::Path;
use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, args_for, args_with};
use meerkat_core::Message;
use rig::Running;
use rkat_tui::daemon::instructions::{UPGRADED_NOTICE, agent_sections, helper_sections};
use serde_json::json;

/// The earlier build's sentence in each of its sections.
const OLD: &str = "OLD-BUILD-LINE";

/// The audit log's lines of a replacement.
fn updates(root: &Path) -> Vec<serde_json::Value> {
    let path = common::realm_of(root).join(rkat_tui::daemon::audit::AUDIT_FILE);
    std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|line| line["event"] == "instructions_updated")
        .collect()
}

/// `system` holds each of `sections` exactly once and none of the
/// earlier build's sentences.
fn current_once(system: &str, sections: &[String]) {
    for section in sections {
        assert_eq!(
            system.matches(section.trim_end()).count(),
            1,
            "{}\n---\n{system}",
            section.lines().next().unwrap_or_default()
        );
    }
    assert!(!system.contains(OLD), "{system}");
}

/// What `rkat/missed` gives the agent's next client.
async fn missed(connection: &rkat_tui::client::transport::Connection) -> Vec<serde_json::Value> {
    let answer = connection.call("rkat/missed", json!({})).await.unwrap();
    answer["notices"].as_array().cloned().unwrap_or_default()
}

fn told_upgrade(notices: &[serde_json::Value]) -> usize {
    notices
        .iter()
        .filter(|notice| notice["text"] == UPGRADED_NOTICE && notice["warn"] != true)
        .count()
}

/// The agent restored on a conversation an earlier build began: its next
/// model request carries this build's sections once and none of the old
/// sentences, the replacement is audited once and its next client told
/// once; a second restart finds them current and sends nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_s_older_instructions_are_replaced_once() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("one")]);
    let daemon = Running::start_with(
        &args_for(&ws.root),
        Running::options(&ws.root, &llm).with_stale_instructions(),
    )
    .await;
    let agent = daemon.agent.clone();
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "first").await;
    assert!(
        llm.recorded()[0].system().contains(OLD),
        "the earlier build's"
    );
    drop(connection);
    daemon.stop().await;
    assert!(updates(&ws.root).is_empty());

    // This build, on the same conversation.
    let llm = ScriptedLlmClient::new(vec![Step::Text("two")]);
    let daemon = Running::start(&ws.root, &llm).await;
    assert!(!daemon.created);
    assert_eq!(daemon.agent, agent);
    let lines = updates(&ws.root);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["agent"], agent.as_str());
    let connection = daemon.connect_bound().await;
    assert_eq!(told_upgrade(&missed(&connection).await), 1);
    assert_eq!(told_upgrade(&missed(&connection).await), 0, "told once");
    daemon.prompt(&connection, "second").await;
    let request = &llm.recorded()[0];
    current_once(&request.system(), &agent_sections(&agent, true));
    // The rest of the prompt stayed: the base prompt and the earlier
    // turn are there.
    assert!(request.system().contains("# Inter-agent Communication"));
    assert!(request.user().contains("first"), "{}", request.user());
    drop(connection);
    daemon.stop().await;

    // A second restart: current, nothing sent, nobody told.
    let llm = ScriptedLlmClient::new(vec![Step::Text("three")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let connection = daemon.connect_bound().await;
    assert_eq!(told_upgrade(&missed(&connection).await), 0);
    assert_eq!(updates(&ws.root).len(), 1);
    daemon.prompt(&connection, "third").await;
    current_once(&llm.recorded()[0].system(), &agent_sections(&agent, true));
    drop(connection);
    daemon.stop().await;
}

/// A compaction after the replacement keeps this build's instructions:
/// the summary's source and the request after it carry them, never the
/// earlier build's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compaction_keeps_the_replaced_instructions() {
    let ws = Workspace::new();
    let compacting = [
        "--set",
        "auto.compact=auto",
        "--set",
        "auto.compact_warn_percent=5",
        "--set",
        "auto.compact_auto_percent=5",
    ];
    let args = args_with(&ws.root, &compacting);
    let llm = ScriptedLlmClient::new(vec![Step::Text("one")]);
    let daemon = Running::start_with(
        &args,
        Running::options(&ws.root, &llm).with_stale_instructions(),
    )
    .await;
    let agent = daemon.agent.clone();
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "first").await;
    drop(connection);
    daemon.stop().await;

    let mut script = vec![Step::Text("ok"); 5];
    script.push(Step::TextUsage("big", 60_000));
    script.push(Step::Text("SUMMARY of the earlier turns"));
    script.push(Step::Text("after"));
    let llm = ScriptedLlmClient::new(script);
    let daemon = Running::start_with(&args, Running::options(&ws.root, &llm)).await;
    let connection = daemon.connect_bound().await;
    for i in 0..6 {
        daemon.prompt(&connection, &format!("turn {i}")).await;
    }
    daemon.prompt(&connection, "next").await;
    let requests = llm.recorded();
    let after = requests.last().unwrap();
    assert!(
        after.user().contains("SUMMARY of the earlier turns"),
        "compacted: {}",
        after.user()
    );
    assert!(!after.user().contains("turn 0"), "{}", after.user());
    current_once(&after.system(), &agent_sections(&agent, true));
    // The summary's source: no earlier build's sentence either.
    let source = &requests[requests.len() - 2];
    assert!(!format!("{:?}", source.messages).contains(OLD));
    drop(connection);
    daemon.stop().await;
}

fn spawn_member(member: &str, kickoff: &str) -> Step {
    common::keep("s1", member, kickoff)
}

/// Whether a recorded request of the helper (its task names `HELPER-TASK`)
/// holds `text`.
fn helper_heard(llm: &ScriptedLlmClient, text: &str) -> bool {
    llm.recorded()
        .iter()
        .filter(|r| r.user().contains("HELPER-TASK"))
        .any(|r| format!("{:?}", r.messages).contains(text))
}

/// A kept helper (`assign` with `keep: true`) across the upgrade: restored, its
/// instructions are brought up at the start (audited as its agent's), and
/// its next model request carries this build's section once and not the
/// earlier build's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_helper_s_older_instructions_are_replaced() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let all = format!("{messages:?}");
        if common::user_texts(messages)
            .concat()
            .contains("HELPER-TASK")
        {
            if all.contains("SAY-IT-ONCE-MORE") {
                return Step::Text("HELPER-AGAIN");
            }
            return Step::Text("IDLE-ANSWER");
        }
        let last = messages
            .last()
            .map(|m| format!("{m:?}"))
            .unwrap_or_default();
        if last.contains("went idle") {
            return Step::Text("NOTED");
        }
        if all.contains("\"m1\"") {
            return Step::Text("sent");
        }
        if let Some(peer) = crate::assign::peer_id(messages, "w1") {
            return Step::Tool {
                id: "m1",
                name: "send_message",
                args: json!({"peer_id": peer, "body": "SAY-IT-ONCE-MORE", "handling_mode": "queue"}),
            };
        }
        if common::user_texts(messages)
            .concat()
            .contains("TELL-IT-AGAIN")
        {
            return Step::Tool {
                id: "p1",
                name: "contacts",
                args: json!({}),
            };
        }
        if matches!(messages.last(), Some(Message::ToolResults { .. })) {
            return Step::Text("spawned");
        }
        spawn_member("w1", "HELPER-TASK: say something")
    }));
    let llm = ScriptedLlmClient::new(vec![step.clone(); 12]);
    let daemon = Running::start_with(
        &args_for(&ws.root),
        Running::options(&ws.root, &llm).with_stale_instructions(),
    )
    .await;
    let agent = daemon.agent.clone();
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| format!("{:?}", r.messages).contains("Your helper `w1` went idle"))
    })
    .await;
    daemon.idle(&agent).await;
    assert!(helper_heard(&llm, OLD), "the earlier build's");
    drop(connection);
    daemon.stop().await;

    let llm = ScriptedLlmClient::new(vec![step; 12]);
    let daemon = Running::start(&ws.root, &llm).await;
    // Brought up at the start, before anything reached it: audited as its
    // agent's.
    let lines = updates(&ws.root);
    assert!(
        lines.iter().any(|line| {
            line["detail"].as_str().is_some_and(|d| d.contains("`w1`")) && line["agent"] == agent
        }),
        "{lines:?}"
    );
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "TELL-IT-AGAIN").await;
    common::until(|| helper_heard(&llm, "SAY-IT-ONCE-MORE")).await;
    let request = llm
        .recorded()
        .into_iter()
        .find(|r| {
            r.user().contains("HELPER-TASK")
                && format!("{:?}", r.messages).contains("SAY-IT-ONCE-MORE")
        })
        .unwrap();
    current_once(&request.system(), &helper_sections(true));
    assert!(format!("{:?}", request.messages).contains("IDLE-ANSWER"));
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| format!("{:?}", r.messages).contains("HELPER-AGAIN"))
    })
    .await;
    daemon.idle(&agent).await;
    drop(connection);
    daemon.stop().await;
}
