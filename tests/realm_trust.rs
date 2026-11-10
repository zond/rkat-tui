//! A realm placed inside the workspace (`--state-root`) has its rules file
//! trusted by digest (item 1(e) of the security fixes): a change rkat-tui
//! made applies at the next start; the same file changed under it (a clone
//! could carry it) gives only its deny rules and a notice naming it, until
//! the user adopts it with `/permissions trust`. The workspace's own realm lives
//! under the config directory, out of the shell's and a clone's reach: its
//! rules file is the user's, a hand edit applies.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use common::{ScriptedLlmClient, Workspace, args_for, args_with};
use rig::Running;
use rkat_tui::approval::rules::{Rule, RuleKind, RuleOrigin};

/// The arguments of a realm inside the workspace `root`.
fn in_workspace(root: &std::path::Path) -> rkat_tui::Args {
    let state = root.join(".rkat");
    args_with(
        root,
        &["--realm", "tui", "--state-root", state.to_str().unwrap()],
    )
}

/// The allow rules' tool patterns the realm layer gives at a start.
fn allowed_at_start(args: &rkat_tui::Args) -> (Vec<String>, Vec<String>) {
    let layers = rkat_tui::bootstrap::read_layers(args).unwrap();
    let allow = layers
        .layered
        .rules
        .allow()
        .map(|(_, rule)| format!("{rule:?}"))
        .collect();
    (allow, layers.notices)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rule_appended_under_the_daemon_is_not_adopted_and_is_named() {
    let ws = Workspace::new();
    let args = in_workspace(&ws.root);
    let llm = ScriptedLlmClient::new(Vec::new());
    let daemon = Running::start_with(&args, Running::options(&ws.root, &llm)).await;
    // A rule the user made through the TUI: written and recorded.
    daemon
        .engine()
        .add_rule(
            Rule::new(
                RuleKind::Allow,
                "shell",
                Some("make lint"),
                RuleOrigin::Realm,
            )
            .unwrap(),
        )
        .unwrap();
    daemon.stop().await;
    let (allow, notices) = allowed_at_start(&args);
    assert!(
        allow.iter().any(|rule| rule.contains("make lint")),
        "the TUI's own rule applies: {allow:?}"
    );
    assert!(notices.is_empty(), "{notices:?}");

    // Something else appends an allow rule to the same file.
    let rules = ws.root.join(".rkat/tui/tui.toml");
    let mut text = std::fs::read_to_string(&rules).unwrap();
    text.push_str("\n[[allow]]\ntool = \"shell\"\narg = \"curl *\"\n\n[[deny]]\ntool = \"shell\"\narg = \"rm *\"\n");
    std::fs::write(&rules, text).unwrap();
    let (allow, notices) = allowed_at_start(&args);
    assert!(
        !allow.iter().any(|rule| rule.contains("curl")),
        "the planted allow rule does not apply: {allow:?}"
    );
    assert!(
        allow.is_empty(),
        "no allow rule of a changed file: {allow:?}"
    );
    let notice = notices.join("\n");
    assert!(notice.contains(&rules.display().to_string()), "{notice}");
    assert!(notice.contains("changed since"), "{notice}");
    let layers = rkat_tui::bootstrap::read_layers(&args).unwrap();
    assert!(
        layers
            .layered
            .rules
            .deny()
            .any(|(_, rule)| format!("{rule:?}").contains("rm")),
        "deny rules still apply"
    );

    // `/permissions trust` adopts it.
    let daemon = Running::start_with(&args, Running::options(&ws.root, &llm)).await;
    daemon.engine().trust_realm_rules().unwrap();
    daemon.stop().await;
    let (allow, notices) = allowed_at_start(&args);
    assert!(allow.iter().any(|rule| rule.contains("curl")), "{allow:?}");
    assert!(notices.is_empty(), "{notices:?}");
}

/// The workspace's own realm is out of every grant of the agent's shell
/// and of a clone's reach: a hand edit of its rules file is the user's and
/// applies, with no notice.
#[test]
fn the_workspace_realms_rules_file_is_the_users() {
    let ws = Workspace::new();
    let args = args_for(&ws.root);
    let rules = common::realm_of(&ws.root).join("tui.toml");
    std::fs::write(&rules, "[[allow]]\ntool = \"shell\"\narg = \"make lint\"\n").unwrap();
    let (allow, notices) = allowed_at_start(&args);
    assert!(
        allow.iter().any(|rule| rule.contains("make lint")),
        "{allow:?}"
    );
    assert!(notices.is_empty(), "{notices:?}");
}
