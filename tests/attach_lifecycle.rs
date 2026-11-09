//! The daemon's lifecycle (P2 of the always-daemon plan,
//! `docs/mob-split.md`): `rkat/status` reports the clients, the detach
//! record and the owner (the client that presents the daemon's start
//! token); `rkat/detach` and an owner's connection ending without
//! `rkat/shutdown` are detaches that keep the daemon running;
//! `rkat/shutdown` stops it and removes its endpoint file.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use std::time::Duration;

use common::{ScriptedLlmClient, TIMEOUT, Workspace, args_for};
use rig::Running;
use rkat_tui::client::remote;
use rkat_tui::client::transport::Connection;
use rkat_tui::daemon::protocol::{self, Status};
use serde_json::json;

/// The start token the tests' daemons were launched with.
const START: &str = "start-token";

async fn owned_daemon(ws: &Workspace, llm: &std::sync::Arc<ScriptedLlmClient>) -> Running {
    Running::start_with(
        &args_for(&ws.root),
        Running::options(&ws.root, llm).with_start_token(START),
    )
    .await
}

async fn attach(daemon: &Running, start_token: Option<&str>, probe: bool) -> Connection {
    let (connection, _notes) = Connection::attach_as(
        &daemon.endpoint(),
        Duration::from_secs(5),
        start_token,
        probe,
    )
    .await
    .unwrap();
    connection
}

async fn status(connection: &Connection) -> Status {
    remote::local(connection, protocol::STATUS, &json!({}))
        .await
        .unwrap()
}

/// Poll `connection`'s status until `pred` holds.
async fn status_until(connection: &Connection, pred: impl Fn(&Status) -> bool) -> Status {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let now = status(connection).await;
        if pred(&now) {
            return now;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out; last status {now:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_reports_the_daemon_its_clients_and_the_owner() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = owned_daemon(&ws, &llm).await;
    let endpoint = daemon.endpoint();
    let owner = attach(&daemon, Some(START), false).await;
    let seen = status(&owner).await;
    assert_eq!(seen.workspace, std::fs::canonicalize(&ws.root).unwrap());
    assert_eq!(seen.pid, endpoint.pid);
    assert_eq!(Some(&seen.started_at), endpoint.started_at.as_ref());
    assert_eq!(seen.session_id.as_ref(), Some(&daemon.session));
    assert_eq!(
        (seen.clients, seen.detached, seen.owner, seen.running),
        (1, false, true, false)
    );
    // Another client, and one presenting a wrong start token: not owners.
    let other = attach(&daemon, None, false).await;
    let wrong = attach(&daemon, Some("not-it"), false).await;
    let seen = status(&other).await;
    assert_eq!((seen.clients, seen.owner), (3, false));
    assert!(!status(&wrong).await.owner);
    // A probe (`--list`) is not counted.
    let probe = attach(&daemon, None, true).await;
    assert_eq!(status(&probe).await.clients, 3);
    drop(probe);
    // A client that is not the owner leaving is no detach.
    drop((other, wrong));
    let seen = status_until(&owner, |s| s.clients == 1).await;
    assert!(!seen.detached);
    drop(owner);
    daemon.stop().await;
}

/// Hangup is detach: the owner's connection dropping without
/// `rkat/shutdown` keeps the daemon and records the detach.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_owner_connection_is_a_detach_and_the_daemon_keeps_running() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = owned_daemon(&ws, &llm).await;
    let owner = attach(&daemon, Some(START), false).await;
    assert!(status(&owner).await.owner);
    drop(owner);
    let watcher = attach(&daemon, None, false).await;
    let seen = status_until(&watcher, |s| s.detached).await;
    assert_eq!(seen.clients, 1);
    assert!(daemon.serving());
    assert!(daemon.endpoint_file.exists());
    // The owner comes back: still the owner, and the detach stays.
    let back = attach(&daemon, Some(START), false).await;
    let seen = status(&back).await;
    assert!(seen.owner && seen.detached);
    drop((watcher, back));
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detach_is_recorded_and_never_reset() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = owned_daemon(&ws, &llm).await;
    let client = attach(&daemon, None, false).await;
    assert!(!status(&client).await.detached);
    let _: serde_json::Value = remote::local(&client, protocol::DETACH, &json!({}))
        .await
        .unwrap();
    assert!(status(&client).await.detached);
    drop(client);
    let later = attach(&daemon, Some(START), false).await;
    assert!(status(&later).await.detached);
    drop(later);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_stops_the_daemon_and_removes_its_endpoint_file() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = owned_daemon(&ws, &llm).await;
    let file = daemon.endpoint_file.clone();
    let owner = attach(&daemon, Some(START), false).await;
    let _: serde_json::Value = remote::local(&owner, protocol::SHUTDOWN, &json!({}))
        .await
        .unwrap();
    drop(owner);
    daemon.ended().await;
    assert!(!file.exists(), "the endpoint file is removed");
}
