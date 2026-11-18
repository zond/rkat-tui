//! The settings framework through the daemon: `/set` and `/unset` at the
//! user's and the workspace's level (`rkat/set_setting`), each through the
//! one writer and the one apply path; `/settings` (`rkat/settings` and its
//! picker); the "always" choices in every workspace; and the refusals that
//! keep the settings files the user's.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use common::{ScriptedLlmClient, Workspace};
use rig::{Attached, Running, Setup};
use rkat_tui::approval::grants::{GrantScope, GrantTarget};
use rkat_tui::ui::{Effect, Msg, PickerKind};
use serde_json::json;

const USER_FILE: &str = "# my own settings\n[ui]\nbell = true # ring\n\n[auto]\n# what the agents do first\ncompact_prepare = \"update PLAN.md\"\n";

async fn set(daemon: &Running, key: &str, value: Option<&str>, user: bool) -> serde_json::Value {
    daemon
        .connect_bound()
        .await
        .call(
            "rkat/set_setting",
            json!({"key": key, "value": value, "user": user}),
        )
        .await
        .unwrap()
}

async fn status(daemon: &Running) -> String {
    daemon
        .connect_bound()
        .await
        .call("rkat/compact_status", json!({}))
        .await
        .unwrap()["text"]
        .as_str()
        .unwrap()
        .to_string()
}

/// `/set --user` writes the user's own `tui.toml` (only its key: the
/// comments and the rest stay), applies at once and says so; the
/// workspace's value wins over it and the answer says that; `/unset`
/// removes each level's again; the next daemon reads the user's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_writes_either_file_keeps_the_rest_and_applies_at_once() {
    let ws = Workspace::new();
    let cfg = common::config_dir_for(&ws.root);
    std::fs::create_dir_all(&cfg).unwrap();
    let user_file = cfg.join("tui.toml");
    std::fs::write(&user_file, USER_FILE).unwrap();
    let realm_file = common::realm_of(&ws.root).join("tui.toml");
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let answer = set(&daemon, "auto.compact_prepare_at", Some("65%"), true).await;
    assert_eq!(answer["ok"], true, "{answer}");
    let text = answer["text"].as_str().unwrap();
    assert!(
        text.starts_with(&format!(
            "auto.compact_prepare_at = 65%: written to your tui.toml ({}); in force: 65% (from your tui.toml); applied at once; the agents are asked at 65% of the window",
            user_file.display()
        )),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(&user_file).unwrap(),
        format!("{USER_FILE}compact_prepare_at = \"65%\"\n")
    );
    assert!(
        status(&daemon).await.contains("asked at 65% of the window"),
        "{}",
        status(&daemon).await
    );
    // The workspace's wins; a user value under it is written, not applied.
    set(&daemon, "auto.compact_prepare_at", Some("300k"), false).await;
    let under = set(&daemon, "auto.compact_prepare_at", Some("50%"), true).await;
    let text = under["text"].as_str().unwrap();
    assert!(
        text.contains("the value in force did not change; this workspace's tui.toml sets it (300000), which wins here: /unset auto.compact_prepare_at removes it"),
        "{text}"
    );
    assert!(realm_file.exists());
    // `/settings` has every layer's value.
    let list = daemon
        .connect_bound()
        .await
        .call("rkat/settings", json!({}))
        .await
        .unwrap();
    let row = list["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["key"] == "auto.compact_prepare_at")
        .unwrap()
        .clone();
    assert_eq!(row["value"], "300000", "{row}");
    assert_eq!(row["source"], "workspace", "{row}");
    assert_eq!(
        row["levels"],
        json!([["user", "50%"], ["workspace", "300000"]]),
        "{row}"
    );
    // Removed from the workspace: the user's applies at once.
    let back = set(&daemon, "auto.compact_prepare_at", None, false).await;
    assert!(
        back["text"]
            .as_str()
            .unwrap()
            .contains("in force: 50% (from your tui.toml); applied at once"),
        "{back}"
    );
    assert!(status(&daemon).await.contains("asked at 50% of the window"));
    daemon.stop().await;
    // The next daemon reads the user's.
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    assert!(
        status(&daemon)
            .await
            .contains("asked at 50% of the window (about"),
        "{}",
        status(&daemon).await
    );
    let gone = set(&daemon, "auto.compact_prepare_at", None, true).await;
    assert!(
        gone["text"]
            .as_str()
            .unwrap()
            .contains("in force: 70% (the default)"),
        "{gone}"
    );
    assert_eq!(std::fs::read_to_string(&user_file).unwrap(), USER_FILE);
    // A bad value or key writes nothing.
    let refused = set(&daemon, "ui.bell", Some("loud"), true).await;
    assert_eq!(refused["ok"], false);
    assert_eq!(refused["text"], "ui.bell takes `true` or `false`");
    assert_eq!(std::fs::read_to_string(&user_file).unwrap(), USER_FILE);
    daemon.stop().await;
}

/// The client's `/settings` picker lists every setting with its value and
/// layer; its change (`u`'s field, then Enter) writes the user's file and
/// the list comes again with the new value and the picker's filter (before,
/// unfiltered, and the highlight landed on another setting); a live `[ui]`
/// setting reaches every attached client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_settings_picker_shows_and_changes_a_setting() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&ws.root, &llm, Setup::default()).await;
    rig.send(Effect::RequestSettingsPicker(Some("ui.".into())));
    let shown = rig
        .wait_for(|msg| matches!(msg, Msg::ShowPicker { picker, .. } if picker.kind == PickerKind::Settings))
        .await;
    let Msg::ShowPicker { picker, .. } = shown else {
        unreachable!()
    };
    let rows: Vec<&str> = picker.rows.iter().map(|row| row.text.as_str()).collect();
    assert_eq!(
        rows,
        vec![
            "ui.helpers  folded  (default)",
            "ui.bell  true  (default)",
            "ui.mouse  true  (default)",
            "ui.checklist  hidden  (default)",
        ]
    );
    rig.send(Effect::PickedSetting {
        key: "ui.checklist".into(),
        value: Some("shown".into()),
        user: true,
        filter: Some("ui.".into()),
    });
    // The confirmation, the daemon's push and the refreshed list come in
    // no fixed order.
    rig.saw(|msg| {
        matches!(msg, Msg::Notice { text, .. } if text.starts_with("ui.checklist = shown: written to your tui.toml"))
    })
    .await;
    // Every client follows the live `[ui]` setting.
    rig.saw(|msg| matches!(msg, Msg::Setting { key, value } if key == "ui.checklist" && value.as_deref() == Some("shown")))
        .await;
    let again = rig
        .saw(|msg| matches!(msg, Msg::ShowPicker { picker, refresh: true } if picker.kind == PickerKind::Settings))
        .await;
    let Msg::ShowPicker { picker: fresh, .. } = again else {
        unreachable!()
    };
    // The list comes again with the filter it was opened with, so the
    // highlight (kept by its index) stays on the setting changed.
    let rows: Vec<&str> = fresh.rows.iter().map(|row| row.text.as_str()).collect();
    assert_eq!(
        rows,
        vec![
            "ui.helpers  folded  (default)",
            "ui.bell  true  (default)",
            "ui.mouse  true  (default)",
            "ui.checklist  shown  (user)",
        ]
    );
    let refreshed = picker.at(3).refreshed(fresh);
    assert_eq!(
        refreshed.selected().map(|row| row.text.as_str()),
        Some("ui.checklist  shown  (user)")
    );
    let user_file = common::config_dir_for(&ws.root).join("tui.toml");
    assert_eq!(
        std::fs::read_to_string(&user_file).unwrap(),
        "[ui]\nchecklist = \"shown\"\n"
    );
    rig.quit().await;
}

/// `/approve`'s "always, in every workspace" and `/permissions add
/// --user` write the user's own `tui.toml`; a grant that reaches the
/// settings files' directories is refused, for a session too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn always_in_every_workspace_writes_the_users_file_and_settings_dirs_are_never_granted() {
    let ws = Workspace::new();
    let granted = ws.sibling("granted");
    std::fs::create_dir_all(&granted).unwrap();
    let granted = std::fs::canonicalize(&granted).unwrap();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let engine = daemon.engine();
    engine
        .grant(
            &daemon.session,
            GrantTarget::Dir {
                path: granted.clone(),
            },
            GrantScope::AlwaysUser,
        )
        .unwrap();
    engine
        .allow_statement(
            &daemon.session,
            "network requests to api.example.com",
            GrantScope::AlwaysUser,
        )
        .unwrap();
    let cfg = common::config_dir_for(&ws.root);
    for dir in [
        cfg.clone(),
        cfg.parent().unwrap().to_path_buf(),
        common::realm_of(&ws.root),
    ] {
        for scope in [
            GrantScope::Session,
            GrantScope::Always,
            GrantScope::AlwaysUser,
        ] {
            let refused = engine
                .grant(
                    &daemon.session,
                    GrantTarget::Dir { path: dir.clone() },
                    scope,
                )
                .unwrap_err();
            assert!(
                refused.contains("holds your rkat-tui settings"),
                "{dir:?} {scope:?}: {refused}"
            );
        }
    }
    drop(engine);
    let added = daemon
        .connect_bound()
        .await
        .call(
            "rkat/rules",
            json!({"op": "add", "deny": true, "tool": "shell", "pattern": "git push*", "user": true}),
        )
        .await
        .unwrap();
    assert_eq!(added["ok"], true, "{added}");
    let user: toml::Table =
        toml::from_str(&std::fs::read_to_string(cfg.join("tui.toml")).unwrap()).unwrap();
    assert_eq!(
        user["confinement"]["extra_write"],
        toml::Value::Array(vec![toml::Value::String(granted.display().to_string())])
    );
    assert_eq!(
        user["auto"]["allow"],
        toml::Value::Array(vec![toml::Value::String(
            "network requests to api.example.com".into()
        )])
    );
    assert_eq!(user["deny"][0]["arg"].as_str(), Some("git push*"));
    // The realm's file has none of them.
    let realm = common::realm_of(&ws.root).join("tui.toml");
    let realm_text = std::fs::read_to_string(&realm).unwrap_or_default();
    assert!(!realm_text.contains("api.example.com"), "{realm_text}");
    // `/settings` reads the user's file again after each.
    let list = daemon
        .connect_bound()
        .await
        .call("rkat/settings", json!({}))
        .await
        .unwrap();
    assert!(list["user_file"].as_str().unwrap().ends_with("tui.toml"));
    daemon.stop().await;
}

/// Two daemons on one user's `tui.toml` (two workspaces, one config
/// directory; the first runs as its own process, the real
/// `rkat-tui-mob`, since two cannot share one process): a `/set --user`
/// through the first reaches the second while it runs, applied (its
/// prepare threshold moves) and told to its client once per setting; a
/// `[ui]` change reaches the client as a value to follow; a hand edit of
/// the workspace's file is followed the same way; a file that does not
/// read is reported once and its last good values stay; the daemon's own
/// write tells its client nothing beyond its confirmation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_daemons_on_one_users_file_follow_each_others_changes_and_hand_edits() {
    let first = Workspace::new();
    let second = Workspace::new();
    let cfg = first.sibling("shared-cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    let user_file = cfg.join("tui.toml");
    // The process daemon reaches no model: an endpoint it never calls.
    std::fs::write(
        &user_file,
        "[endpoints.fake]\ndialect = \"anthropic\"\nbase_url = \"http://127.0.0.1:9\"\napi_key_env = \"HOME\"\n",
    )
    .unwrap();
    let cfg_arg = cfg.to_str().unwrap().to_string();
    let args_a = common::args_with(&first.root, &["--config-dir", &cfg_arg]);
    let file_a = first.sibling("first.endpoint");
    let argv = rkat_tui::client::launch::with_endpoint_file(
        &rkat_tui::client::launch::daemon_argv(&args_a, &first.root),
        &file_a,
    );
    let _first_daemon = tokio::process::Command::new(env!("CARGO_BIN_EXE_rkat-tui-mob"))
        .args(&argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    common::until(|| file_a.exists()).await;
    let mut a = rig::Client::attach_setup(&args_a, None, common::no_lists(), &file_a).await;
    let llm = ScriptedLlmClient::new(vec![]);
    let mut b =
        Attached::start_with(&second.root, &llm, Setup::args(&["--config-dir", &cfg_arg])).await;
    // The first's change, in the second.
    a.send(Effect::SetSettings {
        changes: vec![("auto.compact_prepare_at".into(), Some("65%".into()))],
        user: true,
    });
    a.wait_for(|msg| {
        matches!(msg, Msg::Notice { text, .. } if text.starts_with("auto.compact_prepare_at = 65%: written to your tui.toml"))
    })
    .await;
    let notice = b
        .wait_for(|msg| {
            matches!(msg, Msg::Notice { text, .. } if text.starts_with("auto.compact_prepare_at is now 65%"))
        })
        .await;
    let Msg::Notice { text, .. } = notice else {
        unreachable!()
    };
    assert!(
        text.starts_with(
            "auto.compact_prepare_at is now 65% (from your tui.toml; your tui.toml changed); no instruction is in force; the threshold is 65% of the window"
        ),
        "{text}"
    );
    common::until_async(async || {
        status(&b.daemon)
            .await
            .contains("its threshold is 65% of the window")
    })
    .await;
    // The first's own file kept the endpoint it was started with.
    assert!(
        std::fs::read_to_string(&user_file)
            .unwrap()
            .starts_with("[endpoints.fake]\n")
    );
    // A `[ui]` setting reaches the second's client as a value to follow.
    a.send(Effect::SetSettings {
        changes: vec![("ui.checklist".into(), Some("shown".into()))],
        user: true,
    });
    b.wait_for(|msg| {
        matches!(msg, Msg::Setting { key, value } if key == "ui.checklist" && value.as_deref() == Some("shown"))
    })
    .await;
    // A hand edit of the second's workspace file.
    let realm_file = rkat_tui::bootstrap::resolve_scope(&common::args_with(
        &second.root,
        &["--config-dir", &cfg_arg],
    ))
    .unwrap()
    .realm_root()
    .join("tui.toml");
    std::fs::write(
        &realm_file,
        "# by hand\n[auto]\ncompact_prepare_at = 50000\n",
    )
    .unwrap();
    b.wait_for(|msg| {
        matches!(msg, Msg::Notice { text, .. } if text.starts_with("auto.compact_prepare_at is now 50000 (from this workspace; this workspace's tui.toml changed)"))
    })
    .await;
    common::until_async(async || {
        status(&b.daemon)
            .await
            .contains("its threshold is 50k tokens")
    })
    .await;
    // A broken user file: one warning, the last good values stay.
    let kept = std::fs::read_to_string(&user_file).unwrap();
    std::fs::write(&user_file, "[auto\n").unwrap();
    b.wait_for(|msg| {
        matches!(msg, Msg::Notice { text, .. } if text.starts_with("your tui.toml") && text.contains("does not read, so its last good settings stay in force"))
    })
    .await;
    let list = b
        .daemon
        .connect_bound()
        .await
        .call("rkat/settings", json!({}))
        .await
        .unwrap();
    let checklist = list["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["key"] == "ui.checklist")
        .unwrap()
        .clone();
    assert_eq!(checklist["value"], "shown", "{checklist}");
    std::fs::write(
        &user_file,
        kept.replace("checklist = \"shown\"", "checklist = \"hidden\""),
    )
    .unwrap();
    b.wait_for(|msg| {
        matches!(msg, Msg::Notice { text, .. } if text.starts_with("ui.checklist is now hidden (from your tui.toml; your tui.toml changed)"))
    })
    .await;
    // The second's own write: its confirmation, and no notice of its file
    // changing after the watcher's quiet time.
    b.send(Effect::SetSettings {
        changes: vec![("ui.bell".into(), Some("false".into()))],
        user: false,
    });
    b.wait_for(|msg| {
        matches!(msg, Msg::Notice { text, .. } if text.starts_with("ui.bell = false: written to this workspace's tui.toml"))
    })
    .await;
    tokio::time::sleep(rkat_tui::daemon::settings_watch::DEBOUNCE * 4).await;
    b.send(Effect::SetSettings {
        changes: vec![("ui.bell".into(), Some("true".into()))],
        user: false,
    });
    let next = b.wait_for(|msg| matches!(msg, Msg::Notice { .. })).await;
    assert!(
        matches!(&next, Msg::Notice { text, .. } if text.starts_with("ui.bell = true: written")),
        "{next:?}"
    );
    a.quit().await;
    b.quit().await;
}
