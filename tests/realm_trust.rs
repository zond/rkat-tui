//! The realm's own files are trusted by digest (item 1(e) of the
//! security fixes): a change rkat-tui made applies at the next start; the
//! same file changed under it (the agent's shell can write `.rkat/` on
//! Linux) gives only its deny rules and a notice naming it, until the user
//! adopts it with `/rules trust`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use common::{ScriptedLlmClient, Workspace, args_for};
use rig::Running;
use rkat_tui::approval::rules::{Rule, RuleKind, RuleOrigin};

/// The allow rules' tool patterns the realm layer gives at a start.
fn allowed_at_start(root: &std::path::Path) -> (Vec<String>, Vec<String>) {
    let layers = rkat_tui::bootstrap::read_layers(&args_for(root)).unwrap();
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
    let llm = ScriptedLlmClient::new(Vec::new());
    let daemon = Running::start(&ws.root, &llm).await;
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
    let (allow, notices) = allowed_at_start(&ws.root);
    assert!(
        allow.iter().any(|rule| rule.contains("make lint")),
        "the TUI's own rule applies: {allow:?}"
    );
    assert!(notices.is_empty(), "{notices:?}");

    // The agent's shell appends an allow rule to the same file.
    let rules = common::realm_of(&ws.root).join("tui.toml");
    let mut text = std::fs::read_to_string(&rules).unwrap();
    text.push_str("\n[[allow]]\ntool = \"shell\"\narg = \"curl *\"\n\n[[deny]]\ntool = \"shell\"\narg = \"rm *\"\n");
    std::fs::write(&rules, text).unwrap();
    let (allow, notices) = allowed_at_start(&ws.root);
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
    let layers = rkat_tui::bootstrap::read_layers(&args_for(&ws.root)).unwrap();
    assert!(
        layers
            .layered
            .rules
            .deny()
            .any(|(_, rule)| format!("{rule:?}").contains("rm")),
        "deny rules still apply"
    );

    // `/rules trust` adopts it.
    let daemon = Running::start(&ws.root, &llm).await;
    daemon.engine().trust_realm_rules().unwrap();
    daemon.stop().await;
    let (allow, notices) = allowed_at_start(&ws.root);
    assert!(allow.iter().any(|rule| rule.contains("curl")), "{allow:?}");
    assert!(notices.is_empty(), "{notices:?}");
}
