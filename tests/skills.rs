//! The user's skills directory (`<config dir>/skills`), end to end through
//! the `rkat-tui-mob` daemon with a scripted LLM: its skills are in the
//! skill inventory of the system prompt of every workspace's agent and of
//! a `delegate` helper; a skill of the same name in the project's
//! `.rkat/skills` does not stand in for the user's; a missing directory is
//! no error; `[skills] enabled = false` in the realm config keeps skills
//! off; `/context` names the directories with their counts.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/rig.rs"]
mod rig;

use std::path::Path;

use common::{ScriptedLlmClient, Step, Workspace};
use rig::{Attached, Client, Running, Setup};
use rkat_tui::bootstrap::skills::user_source;
use rkat_tui::ui::{Effect, Msg};
use serde_json::json;

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

/// A flat inventory lists every skill with its source and name (the
/// engine lists collections only above `inventory_threshold`, 12 by
/// default, which its 12 embedded skills already reach).
const FLAT: &str = "[skills]\ninventory_threshold = 100\n";

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
/// first lists it flat (its realm config's `inventory_threshold` is the
/// user's own `[skills]` setting and is kept), the second in the default
/// collection summary, under the user's source.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_user_skill_reaches_the_agents_of_two_workspaces() {
    let first = Workspace::new();
    let second = Workspace::new();
    let cfg = first.sibling("shared-cfg");
    skill(&cfg.join("skills"), "review", "USER-REVIEW-SKILL");
    let setup = || Setup::args(&["--config-dir", cfg.to_str().unwrap()]);
    realm_config(&first.root, &cfg, FLAT);

    let system = first_prompt(&first.root, setup()).await;
    assert!(system.contains(&user_entry("review")), "{system}");
    assert!(system.contains("USER-REVIEW-SKILL"), "{system}");

    let system = first_prompt(&second.root, setup()).await;
    assert!(
        system.contains(&format!(
            "<collection source=\"{}\" count=\"1\">",
            user_source()
        )),
        "{system}"
    );
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
    }
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
    realm_config(&ws.root, &cfg, FLAT);
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
    realm_config(&ws.root, &cfg, FLAT);
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
    assert!(!system.contains("<available_skills"), "{system}");
    assert!(!system.contains("USER-REVIEW-SKILL"), "{system}");
}

/// A `delegate` helper is built by the same factory over the same realm
/// config: its prompt lists the user's skill too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegate_helper_lists_the_users_skills() {
    let ws = Workspace::new();
    let cfg = common::config_dir_for(&ws.root);
    skill(&cfg.join("skills"), "review", "USER-REVIEW-SKILL");
    realm_config(&ws.root, &cfg, FLAT);
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
    rig.quit().await;
}
