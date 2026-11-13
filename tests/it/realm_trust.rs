//! The workspace's realm lives under the config directory, out of the
//! shell's and a clone's reach: its rules file is the user's, and a hand
//! edit applies.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;

use common::{Workspace, args_for};

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
