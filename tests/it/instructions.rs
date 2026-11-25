//! Instruction files and `/remember`, end to end through the
//! `rkat-tui-mob` daemon with a scripted LLM and an attached client: the user-global `SYSTEM.md`
//! replaces the base prompt, the workspace `AGENTS.md` loads natively, and
//! the persona, the user-global `AGENTS.md`, ancestor `AGENTS.md` files
//! (git root found through a `.git` file) and `AGENTS.local.md` (capped)
//! follow in that order; `/remember` appends to the workspace `AGENTS.md`,
//! reaches the live session at once and the next session through the file.
//! The files of `instructions.files` follow the user's own `AGENTS.md`, and
//! a fresh helper gets them too.
//! A conversation the daemon restores after a restart keeps the
//! instructions it was created with: the persona once, edited files only
//! from the next new conversation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

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

/// A daemon started with `--set instructions.files=[…]` puts the file in
/// a new agent's system prompt, after the user's own `AGENTS.md` and
/// before the ancestors, and in a fresh helper's (alone: not the user's
/// `AGENTS.md`); `/context` names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn instruction_files_of_the_command_line_reach_agents_and_fresh_helpers() {
    use meerkat_core::Message;
    let ws = Workspace::new();
    let git_root = ws.root.parent().unwrap();
    write(&git_root.join(".git"), "gitdir: /elsewhere\n");
    write(&git_root.join("AGENTS.md"), "ANCESTOR-RULES apply.\n");
    let global = common::config_dir_for(&ws.root);
    write(&global.join("AGENTS.md"), "GLOBAL-RULES apply.\n");
    let notes = git_root.join("wrapper").join("agent-notes.md");
    write(&notes, "WRAPPER-RULES apply.\n");

    let step = Step::Dynamic(std::sync::Arc::new(|messages: &[Message]| {
        if common::is_helper(messages, "FRESH-TASK") {
            return Step::Text("fresh done");
        }
        if !format!("{messages:?}").contains("\"a1\"") {
            return common::assign("a1", "fresh", "FRESH-TASK");
        }
        Step::Text("ok")
    }));
    let llm = ScriptedLlmClient::new(vec![step; 10]);
    let set = format!("instructions.files=[{:?}]", notes.display().to_string());
    let mut rig = Attached::start_with(&ws.root, &llm, rig::Setup::args(&["--set", &set])).await;
    rig.send(Effect::StartTurn("go".into()));
    let _ = rig.turn_finished().await;
    common::until(|| common::parent_told(&llm, "FRESH-TASK", "fresh")).await;

    let heading = format!("# Instructions (from {})", notes.display());
    let recorded = llm.recorded();
    let agent = recorded[0].system();
    let order = [
        at(&agent, "GLOBAL-RULES"),
        at(&agent, &heading),
        at(&agent, "WRAPPER-RULES"),
        at(&agent, "ANCESTOR-RULES"),
    ];
    assert!(order.is_sorted(), "{order:?}\n{agent}");
    let helper = recorded
        .iter()
        .find(|r| common::is_helper(&r.messages, "FRESH-TASK"))
        .expect("a request of the fresh helper")
        .system();
    assert_eq!(helper.matches(&heading).count(), 1, "{helper}");
    assert!(helper.contains("WRAPPER-RULES apply."), "{helper}");
    assert!(!helper.contains("GLOBAL-RULES"), "{helper}");
    assert!(!helper.contains("ANCESTOR-RULES"), "{helper}");

    rig.send(Effect::ContextReport("context: files".into()));
    let msg = rig
        .wait_for(
            |msg| matches!(msg, Msg::Notice { text, .. } if text.starts_with("context: files")),
        )
        .await;
    let Msg::Notice { text, .. } = msg else {
        unreachable!()
    };
    assert!(
        text.contains(&format!(
            "instructions.files: {} — 20 bytes",
            notes.display()
        )),
        "{text}"
    );
    rig.daemon.idle(&rig.daemon.agent).await;
    rig.quit().await;
}

/// `/set instructions.files` while the daemon runs applies from the next
/// conversation (the one under way keeps its prompt) and warns of a file
/// that does not load.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_set_of_instruction_files_applies_from_the_next_conversation() {
    let ws = Workspace::new();
    let notes = ws.root.parent().unwrap().join("wrapper").join("notes.md");
    write(&notes, "LATER-RULES apply.\n");
    let missing = notes.with_file_name("missing.md");
    let llm = ScriptedLlmClient::new(vec![Step::Text("one"), Step::Text("two")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let first = rig.current_session().await.to_string();
    let value = format!("{}, {}", notes.display(), missing.display());
    let answer = rig
        .daemon
        .connect_bound()
        .await
        .call(
            "rkat/set_setting",
            serde_json::json!({"key": "instructions.files", "value": value, "user": false}),
        )
        .await
        .unwrap();
    assert_eq!(answer["ok"], true, "{answer}");
    let text = answer["text"].as_str().unwrap();
    assert!(
        text.contains("it applies from the next conversation; the conversations and fresh helpers started from now on read 2 instruction files"),
        "{text}"
    );
    let warning = answer["warning"].as_str().unwrap();
    assert!(
        warning.contains(&format!(
            "instructions.files: {} is skipped (there is no such file)",
            missing.display()
        )),
        "{warning}"
    );
    rig.send(Effect::StartTurn("hi".into()));
    let _ = rig.turn_finished().await;
    assert!(!llm.recorded()[0].system().contains("LATER-RULES"));
    rig.send(Effect::NewSession);
    rig.wait_for(|msg| matches!(msg, Msg::Status(StatusUpdate::Session(s)) if *s != first))
        .await;
    rig.send(Effect::StartTurn("again".into()));
    let _ = rig.turn_finished().await;
    let system = llm.recorded()[1].system();
    assert!(system.contains("LATER-RULES apply."), "{system}");
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

/// What the engine's comms section, its `mob-communication` skill and its
/// `send_message` description teach about the comms tools rkat-tui does
/// not offer, none of it in a request.
const ABSENT: [&str; 8] = [
    "skill_name=\"multi-agent-comms\"",
    "reply_to_peer",
    "send_request",
    "send_response",
    "`peers`",
    "**peers**",
    "# Inter-agent Communication",
    "skill_name=\"mob-communication\"",
];

/// A request of `kind` names none of [`ABSENT`], in its system prompt or
/// in `send_message`'s description, and still names `send_message` and
/// `contacts` (its instructions and its tools).
fn no_absent_comms_tools(kind: &str, request: &common::Recorded) {
    let system = request.system();
    for absent in ABSENT {
        assert!(!system.contains(absent), "{kind}: {absent} in\n{system}");
    }
    let send = request
        .defs
        .iter()
        .find(|def| def.name.as_str() == "send_message")
        .unwrap_or_else(|| panic!("{kind}: no send_message in {:?}", request.tools));
    for absent in ["reply_to_peer", "send_request", "send_response", "peers"] {
        assert!(
            !send.description.contains(absent),
            "{kind}: {absent} in {}",
            send.description
        );
    }
    assert!(
        request.tools.iter().any(|tool| tool == "contacts"),
        "{kind}: {:?}",
        request.tools
    );
    for named in ["`send_message`", "`contacts`"] {
        assert!(system.contains(named), "{kind}: no {named} in\n{system}");
    }
}

/// The engine's instructions about `reply_to_peer`, `send_request`,
/// `send_response` and `peers` (lukacf/meerkat#2432) reach no model: not
/// the agent's, a fresh helper's, a fork's (whose conversation is a copy of
/// its parent's, system prompt and all) or a member's of a mob the agent
/// created (`rkat_tui::daemon::comms_text`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_request_teaches_the_comms_tools_we_do_not_offer() {
    use meerkat_core::Message;
    use serde_json::json;
    let ws = Workspace::new();
    let step = Step::Dynamic(std::sync::Arc::new(|messages: &[Message]| {
        let user = common::user_texts(messages).concat();
        if user.contains("FORK-TASK") {
            return Step::Text("fork done");
        }
        if common::is_helper(messages, "FRESH-TASK") {
            return Step::Text("fresh done");
        }
        if common::is_helper(messages, "MOB-TASK") {
            return Step::Text("member done");
        }
        let all = format!("{messages:?}");
        let call = |id: &'static str, name: &'static str, args: serde_json::Value| Step::Tool {
            id,
            name,
            args,
        };
        if !all.contains("\"a1\"") {
            return common::assign("a1", "fresh", "FRESH-TASK");
        }
        if !all.contains("\"a2\"") {
            return common::fork("a2", "kid", "FORK-TASK", false);
        }
        if !all.contains("\"m1\"") {
            return call(
                "m1",
                "mob_create",
                json!({"definition": {"id": "team", "profiles": {"worker": {"model": common::MODEL, "tools": {"comms": true}}}}}),
            );
        }
        if !all.contains("\"s1\"") {
            return call(
                "s1",
                "mob_spawn_member",
                json!({"mob_id": "team", "profile": "worker", "member_id": "w1", "initial_message": "MOB-TASK"}),
            );
        }
        Step::Text("ok")
    }));
    let llm = ScriptedLlmClient::new(vec![step; 30]);
    let setup = rig::Setup::default();
    let args = setup.command_line(&ws.root);
    let daemon = rig::Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    daemon
        .reveal(&daemon.session, &["mob_create", "mob_spawn_member"])
        .await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    // Every helper's run ends before the daemon stops (a fork's result
    // run: lukacf/meerkat#2374, #2384; the team's member: its mob's
    // shutdown).
    common::until(|| common::parent_told(&llm, "FRESH-TASK", "fresh")).await;
    common::until(|| {
        llm.recorded().iter().any(|r| {
            let user = common::user_texts(&r.messages).concat();
            !user.contains("FORK-TASK") && format!("{:?}", r.messages).contains("fork done")
        })
    })
    .await;
    let registry = daemon.registry();
    let member = rkat_tui::mobs::HelperKey::new(
        meerkat_mob::MobId::from("team"),
        meerkat_mob::AgentIdentity::from("w1"),
    );
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| common::is_helper(&r.messages, "MOB-TASK"))
            && registry
                .activity(&member)
                .is_some_and(|(running, _)| !running)
    })
    .await;
    daemon.idle(&daemon.agent).await;

    let recorded = llm.recorded();
    let of = |kind: &str, pick: &dyn Fn(&[Message]) -> bool| {
        let requests: Vec<&common::Recorded> =
            recorded.iter().filter(|r| pick(&r.messages)).collect();
        assert!(!requests.is_empty(), "no request of the {kind}");
        for request in requests {
            no_absent_comms_tools(kind, request);
        }
    };
    let fork = |m: &[Message]| common::user_texts(m).concat().contains("FORK-TASK");
    of("agent", &|m| {
        common::user_texts(m).first().is_some_and(|u| u == "go") && !fork(m)
    });
    of("fresh helper", &|m| common::is_helper(m, "FRESH-TASK"));
    of("fork", &fork);
    of("member of the agent's mob", &|m| {
        common::is_helper(m, "MOB-TASK")
    });
    drop(connection);
    daemon.stop().await;
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
