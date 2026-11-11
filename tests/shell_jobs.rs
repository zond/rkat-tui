//! Background shell jobs on the daemon's agent (P6 item 9): the engine
//! cannot run them for a member with the mob tools (`docs/evidence.md`,
//! "Background shell jobs"), so the job tools are withheld and `shell`
//! with `background` is refused, typed, before the engine sees it. The
//! foreground shell runs as before.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/rig.rs"]
mod rig;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::Attached;
use rkat_tui::approval::policy::{BACKGROUND_SHELL_UNAVAILABLE, SHELL_JOB_TOOLS};
use rkat_tui::ui::Effect;
use serde_json::json;

/// The text of the tool result for call `id` in `messages`.
fn tool_result(messages: &[Message], id: &str) -> String {
    messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResults { results, .. } => results
                .iter()
                .find(|r| r.tool_use_id == id)
                .map(|r| r.text_content()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_background_shell_is_refused_and_the_job_tools_are_withheld() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "bg",
            name: "shell",
            args: json!({"command": "touch background.ran", "background": true}),
        },
        Step::Tool {
            id: "fg",
            name: "shell",
            args: json!({"command": "touch foreground.ran"}),
        },
        Step::Text("done"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("run it in the background".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "done");
    let seen = llm.recorded();

    // The job tools are not offered; the shell is, and its own schema still
    // offers `background` (its job manager has the durable job runtime).
    for tool in SHELL_JOB_TOOLS {
        assert!(!seen[0].tools.iter().any(|t| t == tool), "{tool} offered");
    }
    let shell = seen[0].tools.iter().position(|t| t == "shell").unwrap();
    assert!(
        seen[0].schemas[shell]["properties"]
            .get("background")
            .is_some(),
        "{}",
        seen[0].schemas[shell]
    );

    // The engine refuses it before the gate sees it: the agent's catalog
    // lost the shell's background mode (the engine ask in the evidence;
    // when this changes, the gate's own refusal below applies to the agent
    // too). Nothing ran.
    let refused = tool_result(&seen[1].messages, "bg");
    assert!(
        refused.contains("requested tool execution mode Detached is not supported"),
        "{refused}"
    );
    // The foreground command after it ran: the refusal is the background
    // job's alone.
    assert!(ws.root.join("foreground.ran").exists());
    assert!(!ws.root.join("background.ran").exists());
    rig.quit().await;
}

/// A helper without the mob tools has the shell's background mode; the gate
/// refuses it, typed, with what to do instead, and nothing runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helpers_background_shell_is_refused_by_the_gate() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "d1",
            name: "delegate",
            args: json!({
                "member_id": "builder",
                "task": "build it in the background",
                "result_label": "result",
                "max_text_bytes": 4000,
            }),
        },
        Step::Tool {
            id: "bg",
            name: "shell",
            args: json!({"command": "touch background.ran", "background": true}),
        },
        Step::Text("could not"),
        Step::Text("done"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("delegate a build".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "done");
    let seen = llm.recorded();
    for tool in SHELL_JOB_TOOLS {
        assert!(!seen[1].tools.iter().any(|t| t == tool), "{tool} offered");
    }
    let refused: serde_json::Value =
        serde_json::from_str(&tool_result(&seen[2].messages, "bg")).unwrap();
    assert_eq!(refused["error"], "policy_denied", "{refused}");
    assert_eq!(
        refused["data"]["message"],
        format!("Not available: {BACKGROUND_SHELL_UNAVAILABLE}."),
        "{refused}"
    );
    assert!(!ws.root.join("background.ran").exists());
    rig.quit().await;
}
