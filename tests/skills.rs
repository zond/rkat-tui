//! The user's skills directory (`<config dir>/skills`), end to end through
//! the `rkat-tui-mob` daemon with a scripted LLM: its skills are named
//! with their descriptions in the skill inventory of the system prompt of
//! every workspace's agent and of a `delegate` helper, up to the
//! inventory bound; a skill of the same name in the project's
//! `.rkat/skills` does not stand in for the user's; a missing directory is
//! no error; `[skills] enabled = false` in the realm config keeps skills
//! off; `/context` names the directories with their counts. The agent and
//! a helper open a skill with `skill_read`, in plan mode too.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/rig.rs"]
mod rig;

use std::path::Path;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::{Attached, Client, Running, Setup};
use rkat_tui::approval::mode::PermissionMode;
use rkat_tui::bootstrap::skills::{INVENTORY_BOUND, user_source};
use rkat_tui::tools::{MAX_SKILL_BYTES, SKILL_READ};
use rkat_tui::ui::{Effect, Msg};
use serde_json::json;

/// The text of the tool result `id` in `messages`.
fn tool_result(messages: &[Message], id: &str) -> String {
    messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResults { results, .. } => results
                .iter()
                .find(|r| r.tool_use_id == id)
                .map(meerkat_core::types::ToolResult::text_content),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

/// The agent's `skill_read` call `id` with `args`.
fn read(id: &'static str, args: serde_json::Value) -> Step {
    Step::Tool {
        id,
        name: SKILL_READ,
        args,
    }
}

fn skill(dir: &Path, name: &str, description: &str) {
    let dir = dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\nBody of {name}.\n"),
    )
    .unwrap();
}

/// The realm config of the workspace `root` under the config directory
/// `cfg`, written before its daemon starts.
fn realm_config(root: &Path, cfg: &Path, text: &str) {
    let args = common::args_with(root, &["--config-dir", cfg.to_str().unwrap()]);
    let realm = rkat_tui::bootstrap::resolve_scope(&args)
        .unwrap()
        .realm_root();
    std::fs::write(realm.join("config.toml"), text).unwrap();
}

/// A realm config of the user's own that keeps the inventory to counts
/// per source.
const COUNTS: &str = "[skills]\ninventory_threshold = 5\n";

/// The inventory entry of the user's skill `name`.
fn user_entry(name: &str) -> String {
    format!(
        "<skill source_uuid=\"{}\" skill_name=\"{name}\">",
        user_source()
    )
}

/// The system prompt of the first request of one turn of the daemon on
/// `root` with `setup`.
async fn first_prompt(root: &Path, setup: Setup) -> String {
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut rig = Attached::start_with(root, &llm, setup).await;
    rig.send(Effect::StartTurn("hi".into()));
    let _ = rig.turn_finished().await;
    rig.quit().await;
    llm.recorded()[0].system()
}

/// One user directory, two workspaces: both agents list the skill. The
/// first names it with its description (the realm config leaves the
/// threshold at the engine's default, which rkat-tui raises), the second
/// counts it under the user's source (its realm config's own
/// `inventory_threshold` is kept).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_user_skill_reaches_the_agents_of_two_workspaces() {
    let first = Workspace::new();
    let second = Workspace::new();
    let cfg = first.sibling("shared-cfg");
    skill(&cfg.join("skills"), "review", "USER-REVIEW-SKILL");
    let setup = || Setup::args(&["--config-dir", cfg.to_str().unwrap()]);
    realm_config(&second.root, &cfg, COUNTS);

    let system = first_prompt(&first.root, setup()).await;
    let entry = format!(
        "{}\n    <description>USER-REVIEW-SKILL</description>\n  </skill>",
        user_entry("review")
    );
    assert!(system.contains(&entry), "{system}");
    let inventory = &system[system.find("<available_skills>").unwrap()..];
    eprintln!(
        "{}",
        &inventory[..inventory.find("</available_skills>").unwrap() + 19]
    );
    eprintln!("the user's entry: {} bytes", entry.len() + 1);

    let system = first_prompt(&second.root, setup()).await;
    assert!(
        system.contains(&format!(
            "<collection source=\"{}\" count=\"1\">",
            user_source()
        )),
        "{system}"
    );
    assert!(!system.contains("USER-REVIEW-SKILL"), "{system}");
    // The directory is the daemon's to read: nothing of it was written
    // into either realm's document.
    for ws in [&first, &second] {
        let args = common::args_with(&ws.root, &["--config-dir", cfg.to_str().unwrap()]);
        let realm = rkat_tui::bootstrap::resolve_scope(&args)
            .unwrap()
            .realm_root();
        let text = std::fs::read_to_string(realm.join("config.toml")).unwrap_or_default();
        assert!(!text.contains("skills/"), "{text}");
        assert!(!text.contains(&user_source().to_string()), "{text}");
        assert!(!text.contains(&INVENTORY_BOUND.to_string()), "{text}");
    }
}

/// More skills than the bound: the inventory falls back to counts per
/// source, and `skill_read` without a name lists every one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn past_the_bound_the_inventory_counts_and_the_tool_lists() {
    let ws = Workspace::new();
    let cfg = common::config_dir_for(&ws.root);
    for n in 0..INVENTORY_BOUND {
        skill(
            &cfg.join("skills"),
            &format!("s{n:02}"),
            &format!("SKILL-{n:02}"),
        );
    }
    let llm = ScriptedLlmClient::new(vec![read("r1", json!({})), Step::Text("listed")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("which skills?".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "listed");
    let recorded = llm.recorded();
    let system = recorded[0].system();
    assert!(
        system.contains(&format!(
            "<collection source=\"{}\" count=\"{INVENTORY_BOUND}\">",
            user_source()
        )),
        "{system}"
    );
    assert!(!system.contains("SKILL-07"), "{system}");
    let listed = tool_result(&recorded[1].messages, "r1");
    for n in 0..INVENTORY_BOUND {
        assert!(
            listed.contains(&format!("- s{n:02} [user]: SKILL-{n:02}\n")),
            "{listed}"
        );
    }
    rig.quit().await;
}

/// The agent opens the user's skill: its body, not the project's copy of
/// the name; lists every skill with its source; an unknown name is an
/// error naming what exists; a long body is cut at the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agent_reads_the_users_skill() {
    let ws = Workspace::new();
    let cfg = common::config_dir_for(&ws.root);
    skill(&cfg.join("skills"), "review", "USER-REVIEW-SKILL");
    skill(
        &ws.root.join(".rkat/skills"),
        "review",
        "PROJECT-REVIEW-SKILL",
    );
    let big = cfg.join("skills/big");
    std::fs::create_dir_all(&big).unwrap();
    std::fs::write(
        big.join("SKILL.md"),
        format!(
            "---\nname: big\ndescription: d\n---\n\n{}",
            "x".repeat(MAX_SKILL_BYTES * 2)
        ),
    )
    .unwrap();
    let llm = ScriptedLlmClient::new(vec![
        read("r1", json!({"name": "review"})),
        read("r2", json!({})),
        read("r3", json!({"name": "nope"})),
        read("r4", json!({"name": "big"})),
        Step::Text("read"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("review it".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "read");
    let recorded = llm.recorded();
    assert!(
        recorded[0].tools.iter().any(|t| t == SKILL_READ),
        "{:?}",
        recorded[0].tools
    );
    let body = tool_result(&recorded[1].messages, "r1");
    assert_eq!(
        body,
        "skill `review` [user]: USER-REVIEW-SKILL\n\nBody of review.\n"
    );
    let listed = tool_result(&recorded[2].messages, "r2");
    // The tool lists exactly what the inventory names (the embedded
    // skills gated on a capability included).
    let names = |text: &str, open: &str, close: char| -> Vec<String> {
        let mut names: Vec<String> = text
            .split(open)
            .skip(1)
            .filter_map(|rest| rest.split(close).next().map(str::to_string))
            .collect();
        names.sort();
        names
    };
    let system = recorded[0].system();
    let inventory = &system[system.find("<available_skills>").unwrap()..];
    let inventory = &inventory[..inventory.find("</available_skills>").unwrap()];
    assert_eq!(
        names(inventory, "skill_name=\"", '"'),
        names(&listed, "\n- ", ' '),
        "{listed}"
    );
    assert!(
        listed.contains("- review [user]: USER-REVIEW-SKILL\n"),
        "{listed}"
    );
    assert!(!listed.contains("PROJECT-REVIEW-SKILL"), "{listed}");
    let unknown = tool_result(&recorded[3].messages, "r3");
    assert!(
        unknown.starts_with("no skill named `nope`. Skills: "),
        "{unknown}"
    );
    assert!(unknown.contains("review"), "{unknown}");
    assert!(unknown.contains("big"), "{unknown}");
    let capped = tool_result(&recorded[4].messages, "r4");
    assert!(capped.len() < MAX_SKILL_BYTES + 200, "{}", capped.len());
    assert!(
        capped.ends_with(&format!(
            "[truncated: the skill's body is {} bytes; the first {MAX_SKILL_BYTES} are shown]",
            MAX_SKILL_BYTES * 2
        )),
        "{}",
        &capped[capped.len() - 100..]
    );
    assert_eq!(rig.escalations(), 0);
    rig.quit().await;
}

/// Plan mode allows a skill read without asking anyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_mode_allows_a_skill_read() {
    let ws = Workspace::new();
    let cfg = common::config_dir_for(&ws.root);
    skill(&cfg.join("skills"), "review", "USER-REVIEW-SKILL");
    let llm = ScriptedLlmClient::new(vec![
        read("r1", json!({"name": "review"})),
        Step::Text("planned"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.set_mode(PermissionMode::Plan).await;
    rig.send(Effect::StartTurn("plan the review".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "planned");
    let body = tool_result(&llm.recorded()[1].messages, "r1");
    assert!(body.ends_with("Body of review.\n"), "{body}");
    assert_eq!(rig.escalations(), 0);
    rig.quit().await;
}

/// A project skill of the same name as one of the user's is not listed:
/// the user's is. A project skill of another name still is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_users_skill_wins_over_the_projects_of_the_same_name() {
    let ws = Workspace::new();
    let cfg = common::config_dir_for(&ws.root);
    skill(&cfg.join("skills"), "review", "USER-REVIEW-SKILL");
    skill(
        &ws.root.join(".rkat/skills"),
        "review",
        "PROJECT-REVIEW-SKILL",
    );
    skill(
        &ws.root.join(".rkat/skills"),
        "deploy",
        "PROJECT-DEPLOY-SKILL",
    );
    let system = first_prompt(&ws.root, Setup::default()).await;
    assert!(system.contains("USER-REVIEW-SKILL"), "{system}");
    assert!(!system.contains("PROJECT-REVIEW-SKILL"), "{system}");
    assert!(system.contains("PROJECT-DEPLOY-SKILL"), "{system}");
    assert_eq!(
        system.matches("skill_name=\"review\"").count(),
        1,
        "{system}"
    );
}

/// No user directory: the agent starts and lists the project's skill.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_user_directory_is_no_error() {
    let ws = Workspace::new();
    let cfg = common::config_dir_for(&ws.root);
    skill(
        &ws.root.join(".rkat/skills"),
        "deploy",
        "PROJECT-DEPLOY-SKILL",
    );
    assert!(!cfg.join("skills").exists());
    let system = first_prompt(&ws.root, Setup::default()).await;
    assert!(system.contains("PROJECT-DEPLOY-SKILL"), "{system}");
    assert!(!system.contains(&user_source().to_string()), "{system}");
}

/// `[skills] enabled = false` in the realm config: no inventory at all,
/// the user's skills included.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn skills_off_in_the_realm_config_stay_off() {
    let ws = Workspace::new();
    let cfg = common::config_dir_for(&ws.root);
    skill(&cfg.join("skills"), "review", "USER-REVIEW-SKILL");
    realm_config(&ws.root, &cfg, "[skills]\nenabled = false\n");
    let system = first_prompt(&ws.root, Setup::default()).await;
    // (The persona's hint line names the tag; no inventory follows it.)
    assert!(!system.contains("<available_skills>\n"), "{system}");
    assert!(!system.contains("<available_skills mode"), "{system}");
    assert!(!system.contains("USER-REVIEW-SKILL"), "{system}");
}

/// A `delegate` helper is built by the same factory over the same realm
/// config: its prompt lists the user's skill too, and it reads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegate_helper_lists_the_users_skills() {
    let ws = Workspace::new();
    let cfg = common::config_dir_for(&ws.root);
    skill(&cfg.join("skills"), "review", "USER-REVIEW-SKILL");
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "d1",
            name: "delegate",
            args: json!({
                "member_id": "scout",
                "task": "HELPER-TASK: look around",
                "result_label": "result",
                "max_text_bytes": 4000,
            }),
        },
        read("h1", json!({"name": "review"})),
        Step::Text("HELPER-DONE"),
        Step::Text("parent done"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("delegate it".into()));
    let _ = client.turn_finished().await;
    let recorded = llm.recorded();
    let helper = recorded
        .iter()
        .find(|r| r.user().contains("HELPER-TASK"))
        .expect("the helper's request");
    let system = helper.system();
    assert!(system.contains(&user_entry("review")), "{system}");
    assert!(system.contains("USER-REVIEW-SKILL"), "{system}");
    assert!(
        helper.tools.iter().any(|t| t == SKILL_READ),
        "{:?}",
        helper.tools
    );
    let body = recorded
        .iter()
        .find(|r| r.user().contains("HELPER-TASK") && r.messages.len() > helper.messages.len())
        .map(|r| tool_result(&r.messages, "h1"))
        .expect("the helper's request after its read");
    assert!(body.ends_with("Body of review.\n"), "{body}");
    client.quit().await;
    daemon.stop().await;
}

/// `/context` names the user's directory and the project's, each with
/// its skills.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_names_the_skills_directories() {
    let ws = Workspace::new();
    let cfg = common::config_dir_for(&ws.root);
    skill(&cfg.join("skills"), "review", "d");
    skill(&cfg.join("skills"), "deploy", "d");
    skill(&ws.root.join(".rkat/skills"), "lint", "d");
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("hi".into()));
    let _ = rig.turn_finished().await;
    rig.send(Effect::ContextReport("context: header".into()));
    let msg = rig
        .wait_for(
            |msg| matches!(msg, Msg::Notice { text, .. } if text.starts_with("context: header")),
        )
        .await;
    let Msg::Notice { text, .. } = msg else {
        unreachable!()
    };
    assert!(
        text.contains(&format!(
            "  your skills: {} — 2 skills",
            cfg.join("skills").display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "  project skills: {} — 1 skill",
            ws.root.join(".rkat/skills").display()
        )),
        "{text}"
    );
    // Meerkat's 12 embedded skills, the user's two and the project's one.
    assert!(
        text.contains(&format!(
            "  the agent's inventory names all 15 skills with their descriptions (up to {INVENTORY_BOUND}); it opens one with skill_read"
        )),
        "{text}"
    );
    rig.quit().await;
}
