//! What the TUI writes to the workspace's realm `tui.toml` comes back
//! after the daemon restarts, whichever way it stopped: the rig's stop
//! (the signal path), `rkat/shutdown` (a `/quit` of the last client) and
//! a retiring `rkat/shutdown`. Each setting is made through the method
//! or the engine call its command uses, the file holds every key between
//! the two runs (no writer drops another's), and the next daemon on the
//! same workspace and arguments has it in force.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use common::{ScriptedLlmClient, Workspace};
use rig::{Running, Setup};
use rkat_tui::approval::grants::{GrantScope, GrantTarget};
use rkat_tui::approval::mode::PermissionMode;
use rkat_tui::approval::statements::StatementOrigin;
use rkat_tui::cli::AttachTarget;
use rkat_tui::config::ChecklistView;
use serde_json::json;
use std::path::{Path, PathBuf};

const INSTRUCTION: &str = "update PLAN.md with where you are";
const STATEMENT: &str = "network requests to api.example.com";
const CLASSIFIER: &str = "anthropic:claude-haiku-5-5";

/// How the first daemon stops.
#[derive(Debug, Clone, Copy)]
enum Stop {
    /// The rig's stop: the daemon's stop signal, as after SIGTERM.
    Signal,
    /// `rkat/shutdown`, as the last client's `/quit` sends it.
    Shutdown,
    /// `rkat/shutdown` retiring every agent.
    Retiring,
}

/// A daemon whose classifier is the bootstrap's own (so `/auto model` can
/// switch it), on the scripted LLM.
fn setup() -> Setup {
    Setup {
        llm_classifier: true,
        ..Setup::default()
    }
}

async fn start(ws: &Workspace) -> Running {
    let setup = setup();
    let llm = ScriptedLlmClient::new(vec![]);
    Running::start_with(&setup.command_line(&ws.root), setup.options(&ws.root, &llm)).await
}

async fn call(daemon: &Running, method: &str, params: serde_json::Value) -> serde_json::Value {
    daemon
        .connect_bound()
        .await
        .call(method, params)
        .await
        .unwrap()
}

/// The realm's `tui.toml` as stored.
fn stored(realm_toml: &Path) -> toml::Table {
    toml::from_str(&std::fs::read_to_string(realm_toml).unwrap()).unwrap()
}

/// Every key the TUI writes, as the file holds it.
fn assert_stored(realm_toml: &Path, granted: &Path, way: Stop) {
    let file = stored(realm_toml);
    let auto = file["auto"].as_table().unwrap();
    assert_eq!(
        auto["compact_prepare"].as_str(),
        Some(INSTRUCTION),
        "{way:?}: {file}"
    );
    assert_eq!(
        auto["compact_prepare_at"].as_str(),
        Some("65%"),
        "{way:?}: {file}"
    );
    assert_eq!(
        auto["classifier_model"].as_str(),
        Some(CLASSIFIER),
        "{way:?}: {file}"
    );
    assert_eq!(
        auto["mode"].as_str(),
        Some("accept-edits"),
        "{way:?}: {file}"
    );
    assert_eq!(
        auto["allow"].as_array().unwrap(),
        &vec![toml::Value::String(STATEMENT.into())],
        "{way:?}: {file}"
    );
    assert_eq!(
        file["confinement"]["extra_write"].as_array().unwrap(),
        &vec![toml::Value::String(granted.to_str().unwrap().into())],
        "{way:?}: {file}"
    );
    assert_eq!(
        file["ui"]["checklist"].as_str(),
        Some("shown"),
        "{way:?}: {file}"
    );
}

/// Set everything through the paths the commands take, stop the daemon
/// `way`, and check the next daemon has all of it.
async fn survives(way: Stop) {
    let ws = Workspace::new();
    let granted: PathBuf = ws.sibling("granted");
    std::fs::create_dir_all(&granted).unwrap();
    let granted = std::fs::canonicalize(&granted).unwrap();
    let realm_toml = common::realm_of(&ws.root).join("tui.toml");
    let daemon = start(&ws).await;
    let before = call(&daemon, "rkat/compact_status", json!({})).await;
    assert!(
        before["text"]
            .as_str()
            .unwrap()
            .starts_with("compact prepare: none"),
        "{before}"
    );
    // `/compact prepare at 65%`, then `/compact prepare <text>`.
    call(
        &daemon,
        "rkat/set_setting",
        json!({"key": "auto.compact_prepare_at", "value": "65%"}),
    )
    .await;
    let set = call(
        &daemon,
        "rkat/set_setting",
        json!({"key": "auto.compact_prepare", "value": INSTRUCTION}),
    )
    .await;
    assert_eq!(set["ok"], true, "{set}");
    // `/auto model`.
    let switched = call(
        &daemon,
        "rkat/set_setting",
        json!({"key": "auto.classifier_model", "value": CLASSIFIER}),
    )
    .await;
    assert_eq!(switched["ok"], true, "{switched}");
    assert!(switched.get("warning").is_none(), "{switched}");
    // Shift+Tab's mode, Ctrl+T's checklist view.
    call(&daemon, "rkat/set_mode", json!({"mode": "accept-edits"})).await;
    call(
        &daemon,
        "rkat/set_setting",
        json!({"key": "ui.checklist", "value": "shown"}),
    )
    .await;
    // `/approve`'s "always" for a directory and for a kind of action.
    let engine = daemon.engine();
    engine
        .grant(
            &daemon.session,
            GrantTarget::Dir {
                path: granted.clone(),
            },
            GrantScope::Always,
        )
        .unwrap();
    engine
        .allow_statement(&daemon.session, STATEMENT, GrantScope::Always)
        .unwrap();
    drop(engine);
    let status = call(&daemon, "rkat/compact_status", json!({})).await;
    let status = status["text"].as_str().unwrap();
    assert!(
        status.starts_with(&format!(
            "compact prepare: \"{INSTRUCTION}\", from this workspace; asked at 65% of the window"
        )) && status.contains(": armed"),
        "{status}"
    );
    assert_stored(&realm_toml, &granted, way);
    match way {
        Stop::Signal => daemon.stop().await,
        Stop::Shutdown | Stop::Retiring => {
            let retire = matches!(way, Stop::Retiring);
            call(&daemon, "rkat/shutdown", json!({"retire": retire})).await;
            daemon.ended().await;
        }
    }
    // Between the runs: nothing removed or rewrote a key.
    assert_stored(&realm_toml, &granted, way);

    // The next daemon on the same workspace and arguments.
    let daemon = start(&ws).await;
    let status = call(&daemon, "rkat/compact_status", json!({})).await;
    let status = status["text"].as_str().unwrap();
    assert!(
        status.starts_with(&format!(
            "compact prepare: \"{INSTRUCTION}\", from this workspace; asked at 65% of the window"
        )) && status.contains(": armed"),
        "{way:?}: {status}"
    );
    let engine = daemon.engine();
    assert_eq!(
        engine.reviewer().model().as_deref(),
        Some(CLASSIFIER),
        "{way:?}"
    );
    assert_eq!(engine.mode(), PermissionMode::AcceptEdits, "{way:?}");
    assert!(
        engine
            .statements()
            .rows(None)
            .iter()
            .any(|row| row.text == STATEMENT && row.origin == StatementOrigin::Always),
        "{way:?}: {:?}",
        engine.statements().rows(None)
    );
    assert!(
        engine.grants().rows(None).iter().any(|row| row.target
            == GrantTarget::Dir {
                path: granted.clone()
            }),
        "{way:?}: {:?}",
        engine.grants().rows(None)
    );
    drop(engine);
    // The client reads the checklist view from the same layers.
    let client =
        rkat_tui::client::configure(&setup().command_line(&ws.root), AttachTarget::Existing)
            .await
            .unwrap();
    assert_eq!(client.ui.checklist, ChecklistView::Shown, "{way:?}");
    assert_stored(&realm_toml, &granted, way);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_set_settings_survive_a_daemon_stop() {
    survives(Stop::Signal).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_set_settings_survive_a_shutdown() {
    survives(Stop::Shutdown).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_set_settings_survive_a_retiring_shutdown() {
    survives(Stop::Retiring).await;
}
