//! Instruction files and `/remember`, end to end through the
//! `rkat-tui-mob` daemon with a scripted LLM and an attached client: the user-global `SYSTEM.md`
//! replaces the base prompt, the workspace `AGENTS.md` loads natively, and
//! the persona, the user-global `AGENTS.md`, ancestor `AGENTS.md` files
//! (git root found through a `.git` file) and `AGENTS.local.md` (capped)
//! follow in that order; `/remember` appends to the workspace `AGENTS.md`,
//! reaches the live session at once and the next session through the file.
//! A conversation the daemon restores after a restart keeps the
//! instructions it was created with: the persona once, edited files only
//! from the next new conversation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/rig.rs"]
mod rig;

use std::path::Path;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::prompt::AGENTS_MD_MAX_BYTES;
use rig::Attached;
use rkat_tui::ui::{Effect, Msg, NoticeLevel, StatusUpdate};

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Position of `needle` in `haystack`, failing with the haystack.
fn at(haystack: &str, needle: &str) -> usize {
    haystack
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` missing from the system prompt:\n{haystack}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn instruction_files_reach_the_prompt_in_order() {
    let ws = Workspace::new();
    // The git root is the workspace's parent, marked by a `.git` file (a
    // worktree); the workspace is `<root>/project`.
    let git_root = ws.root.parent().unwrap();
    write(&git_root.join(".git"), "gitdir: /elsewhere\n");
    write(&git_root.join("AGENTS.md"), "ANCESTOR-RULES apply.\n");
    write(&ws.root.join("AGENTS.md"), "WORKSPACE-RULES apply.\n");
    let local = format!("LOCAL-RULES apply.\n{}", "x".repeat(AGENTS_MD_MAX_BYTES));
    write(&ws.root.join("AGENTS.local.md"), &local);
    let global = common::config_dir_for(&ws.root);
    write(&global.join("SYSTEM.md"), "You are TESTPERSONA.\n");
    write(&global.join("AGENTS.md"), "GLOBAL-RULES apply.\n");

    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("hi".into()));
    let _ = rig.turn_finished().await;

    let system = llm.recorded()[0].system();
    let order = [
        at(&system, "You are TESTPERSONA."),
        at(&system, "WORKSPACE-RULES"),
        at(&system, "interactive coding agent"),
        at(&system, "GLOBAL-RULES"),
        at(&system, "ANCESTOR-RULES"),
        at(&system, "LOCAL-RULES"),
    ];
    assert!(order.is_sorted(), "{order:?}\n{system}");
    assert!(system.contains(&format!(
        "# Instructions (from {})",
        git_root.join("AGENTS.md").display()
    )));
    assert!(system.contains("[truncated at 32 KiB]"), "{system}");
    // SYSTEM.md replaced the base prompt.
    assert!(
        !system.contains(meerkat_core::prompt::DEFAULT_SYSTEM_PROMPT),
        "{system}"
    );
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remember_reaches_this_session_and_the_next() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("one"), Step::Text("two")]);
    let mut rig = Attached::start(&ws.root, &llm).await;

    rig.send(Effect::Remember("  prefer tabs\n over spaces ".into()));
    let msg = rig
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("remembered")))
        .await;
    assert!(
        matches!(&msg, Msg::Notice { level: NoticeLevel::Info, text } if text.contains("next session")),
        "{msg:?}"
    );
    // The same line again is one system message.
    rig.send(Effect::Remember("prefer tabs over spaces".into()));
    rig.wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("remembered")))
        .await;
    assert_eq!(
        std::fs::read_to_string(ws.root.join("AGENTS.md")).unwrap(),
        "# AGENTS.md\n\n- prefer tabs over spaces\n- prefer tabs over spaces\n"
    );

    rig.send(Effect::StartTurn("hi".into()));
    let _ = rig.turn_finished().await;
    let system = llm.recorded()[0].system();
    assert_eq!(
        system
            .matches("# Remembered instruction\n\nprefer tabs over spaces")
            .count(),
        1,
        "{system}"
    );

    // A new session loads it with the workspace AGENTS.md.
    let first = rig.current_session().await.to_string();
    rig.send(Effect::NewSession);
    rig.wait_for(|msg| matches!(msg, Msg::Status(StatusUpdate::Session(id)) if *id != first))
        .await;
    rig.send(Effect::StartTurn("again".into()));
    let _ = rig.turn_finished().await;
    let system = llm.recorded()[1].system();
    assert!(system.contains("- prefer tabs over spaces"), "{system}");
    assert!(!system.contains("# Remembered instruction\n\n"), "{system}");
    rig.quit().await;
}

/// The daemon restores its agent on the conversation it had: the
/// instructions it was created with stay (the edited file is not pushed
/// into it, the persona appears once); a new conversation reads the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resumed_session_keeps_its_instructions_and_the_persona_once() {
    let ws = Workspace::new();
    write(&ws.root.join("AGENTS.local.md"), "FIRST-RULES apply.\n");
    let llm = ScriptedLlmClient::new(vec![Step::Text("one")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let id = rig.current_session().await;
    rig.send(Effect::StartTurn("hi".into()));
    let _ = rig.turn_finished().await;
    let system = llm.recorded()[0].system();
    assert!(system.contains("FIRST-RULES"), "{system}");
    rig.quit().await;

    // The file changes; the daemon restarts on the same conversation.
    write(&ws.root.join("AGENTS.local.md"), "SECOND-RULES apply.\n");
    let llm = ScriptedLlmClient::new(vec![Step::Text("two"), Step::Text("three")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    assert_eq!(rig.current_session().await, id);
    rig.send(Effect::StartTurn("again".into()));
    let _ = rig.turn_finished().await;
    let system = llm.recorded()[0].system();
    assert_eq!(
        system.matches("interactive coding agent").count(),
        1,
        "{system}"
    );
    assert!(system.contains("FIRST-RULES"), "{system}");
    assert!(!system.contains("SECOND-RULES"), "{system}");

    // A new conversation reads the edited file.
    rig.send(Effect::NewSession);
    let first = id.to_string();
    rig.wait_for(|msg| matches!(msg, Msg::Status(StatusUpdate::Session(s)) if *s != first))
        .await;
    rig.send(Effect::StartTurn("fresh".into()));
    let _ = rig.turn_finished().await;
    let system = llm.recorded()[1].system();
    assert!(system.contains("SECOND-RULES"), "{system}");
    rig.quit().await;
}
