#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Drift pins: local copies of upstream (`meerkat-tools`) argument types,
//! checked against the live upstream tools so an upstream change fails here
//! instead of silently desynchronizing the approval policy.

use meerkat_tools::builtin::BuiltinTool;
use meerkat_tools::builtin::shell::{ShellConfig, ShellTool};
use rkat_tui::approval::policy::ShellArgs;
use serde_json::json;
use std::collections::BTreeSet;

// ---------------------------------------------------------------- shell ----

/// The field names `ShellArgs` accepts, read from its own
/// `deny_unknown_fields` error (it derives no schema).
fn shell_args_fields() -> BTreeSet<String> {
    let err = serde_json::from_value::<ShellArgs>(json!({"command": "x", "zz_unknown": 1}))
        .expect_err("ShellArgs must deny unknown fields");
    let message = err.to_string();
    let (_, expected) = message
        .split_once("expected one of ")
        .unwrap_or_else(|| panic!("unexpected serde message: {message}"));
    expected
        .split(',')
        .map(|field| field.trim().trim_matches('`').to_string())
        .filter(|field| !field.is_empty())
        .collect()
}

#[test]
fn shell_args_match_the_live_shell_tool_schema() {
    let def = ShellTool::new(ShellConfig::default()).def();
    assert_eq!(def.name.as_str(), "shell");
    let schema = &def.input_schema;
    let properties: BTreeSet<String> = schema["properties"]
        .as_object()
        .expect("shell schema has properties")
        .keys()
        .cloned()
        .collect();
    let required: BTreeSet<String> = schema["required"]
        .as_array()
        .expect("shell schema has a required list")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();

    let local = shell_args_fields();
    assert_eq!(
        local,
        ["command", "working_dir", "timeout_secs", "background"]
            .map(String::from)
            .into_iter()
            .collect::<BTreeSet<_>>(),
        "ShellArgs' own field set changed"
    );
    // Upstream lists `background` only when its job manager exports
    // canonical async ops (a durable job runtime bound to a session); every
    // other field is always listed.
    let mut upstream = properties;
    upstream.insert("background".to_string());
    assert_eq!(
        upstream, local,
        "upstream shell input fields drifted from crate::approval::policy::ShellArgs"
    );
    assert_eq!(
        required,
        BTreeSet::from(["command".to_string()]),
        "upstream shell required fields drifted"
    );
    // ShellArgs requires exactly `command` too.
    assert!(serde_json::from_value::<ShellArgs>(json!({})).is_err());
    assert!(serde_json::from_value::<ShellArgs>(json!({"command": "ls"})).is_ok());
}
