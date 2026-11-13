//! `watch_paths` (P6 item B) end to end through the `rkat-tui-mob` daemon
//! with a scripted LLM: the daemon's process watches the workspace, and a
//! change of a watched file reaches the agent as a message
//! (`files changed: …`), labelled in its TUI. The tests wait on explicit
//! signals (a scripted call, the client's notice), never on a sleep.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::Attached;
use rkat_tui::ui::{Effect, Msg};
use serde_json::json;

/// Whether `m` announces a watch event naming `path`.
fn changed(m: &Msg, path: &str) -> bool {
    matches!(m, Msg::Triggered { label, prompt } if label == "files changed" && prompt.contains(path))
}

/// `/status`'s line of this agent's watches (`None`: it has none).
async fn watches_line(rig: &mut Attached) -> Option<String> {
    rig.send(Effect::StatusReport);
    let Msg::Notice { text, .. } = rig
        .wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.contains("approval gate:")))
        .await
    else {
        unreachable!()
    };
    text.lines()
        .find(|line| line.starts_with("file watches: "))
        .map(str::to_string)
}

/// The texts of `messages`' user messages.
fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User(user) => Some(user.text_content()),
            _ => None,
        })
        .collect()
}

/// A watch on `src/**` for the turn: the agent's own `write_file` is not
/// reported back, a file written by someone else is, as one message the
/// agent reads at its next step and its TUI shows labelled; after the
/// turn the watch is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watched_change_reaches_the_agent_in_its_turn() {
    let ws = Workspace::new();
    std::fs::create_dir_all(ws.root.join("src")).unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "w1",
            name: "watch_paths",
            args: json!({"patterns": ["src/**"]}),
        },
        Step::Tool {
            id: "f1",
            name: "write_file",
            args: json!({"path": "src/own.rs", "content": "fn own() {}\n"}),
        },
        // Held until the test has seen the event announced.
        Step::Gated(
            Arc::clone(&gate),
            Box::new(Step::Tool {
                id: "g1",
                name: "glob",
                args: json!({"pattern": "src/*"}),
            }),
        ),
        Step::Text("saw it"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("tell me when src changes".into()));
    // The gated call is open: the watch exists and the own write is done.
    llm.wait_call(2).await;
    assert_eq!(
        watches_line(&mut rig).await.as_deref(),
        Some("file watches: watch 1: src/**")
    );
    std::fs::write(ws.root.join("src/theirs.rs"), "fn theirs() {}\n").unwrap();
    rig.wait_for(|m| changed(m, "src/theirs.rs")).await;
    gate.notify_one();
    assert_eq!(rig.turn_finished().await.unwrap().text, "saw it");
    let seen = user_texts(&llm.requests()[3]);
    assert!(
        seen.iter()
            .any(|text| text == "files changed: src/theirs.rs"),
        "{seen:?}"
    );
    assert!(
        !rig.seen.iter().any(|m| changed(m, "own.rs")),
        "the agent's own write was not reported"
    );
    // The turn ended: the watch went with it.
    assert_eq!(watches_line(&mut rig).await, None);
    rig.quit().await;
}

/// A `once` watch outlives the turn that made it: its change starts a
/// turn of the agent's own (labelled, recorded in the audit log as the
/// watch's), and the watch is gone after that one event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_once_watch_wakes_the_agent_once() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "w1",
            name: "watch_paths",
            args: json!({"patterns": ["out.txt"], "once": true}),
        },
        Step::Text("waiting"),
        Step::Text("noticed"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("wake me when out.txt appears".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "waiting");
    // It outlived the turn.
    assert_eq!(
        watches_line(&mut rig).await.as_deref(),
        Some("file watches: watch 1: out.txt (once)")
    );
    std::fs::write(ws.root.join("out.txt"), "done\n").unwrap();
    rig.wait_for(|m| changed(m, "out.txt")).await;
    assert_eq!(rig.turn_finished().await.unwrap().text, "noticed");
    assert_eq!(
        user_texts(&llm.requests()[2]).last().map(String::as_str),
        Some("files changed: out.txt")
    );
    let log = common::realm_of(&ws.root).join(rkat_tui::daemon::audit::AUDIT_FILE);
    let triggered = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .filter(|line| {
            line.contains("\"run_triggered\"") && line.contains("watch (files changed: out.txt)")
        })
        .count();
    assert_eq!(triggered, 1);
    // Once: the watch is gone after its event.
    assert_eq!(watches_line(&mut rig).await, None);
    rig.quit().await;
}
