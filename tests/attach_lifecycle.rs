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

use common::{ScriptedLlmClient, Step, TIMEOUT, Workspace, args_for};
use rig::{Client, Running};
use rkat_tui::client::remote;
use rkat_tui::client::runner::quit_decision;
use rkat_tui::client::transport::Connection;
use rkat_tui::daemon::protocol::{self, Status};
use rkat_tui::ui::{Departure, Effect, Msg, QuitDecision};
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

// The attach runner's `/quit`, `/detach` and hangup against a daemon.

fn decided(msg: &Msg) -> Option<&QuitDecision> {
    match msg {
        Msg::QuitDecided(decision) => Some(decision),
        _ => None,
    }
}

/// `/quit`'s decision for `client`.
async fn decide(client: &mut Client) -> QuitDecision {
    client.send(Effect::DecideQuit);
    let msg = client.wait_for(|m| decided(m).is_some()).await;
    decided(&msg).cloned().unwrap()
}

/// Leave as `departure` (none: a hangup) and wait for the runner to end.
async fn leave(client: Client, departure: Option<Departure>) {
    if let Some(departure) = departure {
        client.send(Effect::Leave(departure));
    }
    client.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quit_stops_the_daemon_this_client_started() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = owned_daemon(&ws, &llm).await;
    let file = daemon.endpoint_file.clone();
    let mut client = Client::attach_with(&ws.root, Some(START)).await;
    assert_eq!(decide(&mut client).await, QuitDecision::Terminate);
    client.send(Effect::Leave(Departure::Terminate));
    client
        .wait_for(
            |m| matches!(m, Msg::Notice { text, .. } if text.starts_with("stopped the daemon")),
        )
        .await;
    client.quit().await;
    daemon.ended().await;
    assert!(!file.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detach_keeps_the_daemon_and_a_later_quit_asks() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = owned_daemon(&ws, &llm).await;
    let client = Client::attach_with(&ws.root, Some(START)).await;
    leave(client, Some(Departure::Detach)).await;
    assert!(daemon.serving());
    // The owner is back: a detach happened, so /quit asks.
    let mut client = Client::attach_with(&ws.root, Some(START)).await;
    let QuitDecision::Ask { reason, running } = decide(&mut client).await else {
        panic!("not asked");
    };
    assert!(reason.contains("detached"), "{reason}");
    assert!(!running);
    leave(client, Some(Departure::Detach)).await;
    assert!(daemon.serving());
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quit_with_another_client_attached_leaves_the_daemon_to_it() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = owned_daemon(&ws, &llm).await;
    let mut owner = Client::attach_with(&ws.root, Some(START)).await;
    let mut other = Client::attach(&ws.root).await;
    assert_eq!(decide(&mut owner).await, QuitDecision::Leave);
    assert!(
        owner.notices().contains("1 other client is attached"),
        "{}",
        owner.notices()
    );
    leave(owner, None).await;
    assert!(daemon.serving());
    // The owner left it running: the other client is asked.
    let watcher = attach(&daemon, None, true).await;
    status_until(&watcher, |s| s.clients == 1 && s.detached).await;
    let QuitDecision::Ask { reason, .. } = decide(&mut other).await else {
        panic!("not asked");
    };
    assert!(reason.contains("detached"), "{reason}");
    leave(other, Some(Departure::Terminate)).await;
    daemon.ended().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_did_not_start_the_daemon_is_asked() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = owned_daemon(&ws, &llm).await;
    let mut client = Client::attach(&ws.root).await;
    let QuitDecision::Ask { reason, .. } = decide(&mut client).await else {
        panic!("not asked");
    };
    assert!(reason.contains("did not start"), "{reason}");
    leave(client, Some(Departure::Detach)).await;
    daemon.stop().await;
}

/// A hangup (the UI quits without a departure) keeps the daemon and
/// counts as the owner's detach.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_owner_hanging_up_keeps_the_daemon() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = owned_daemon(&ws, &llm).await;
    let client = Client::attach_with(&ws.root, Some(START)).await;
    leave(client, None).await;
    let watcher = attach(&daemon, None, true).await;
    status_until(&watcher, |s| s.clients == 0 && s.detached).await;
    assert!(daemon.serving());
    drop(watcher);
    daemon.stop().await;
}

#[test]
fn the_quit_decision_follows_the_rules() {
    let status = |clients, detached, owner, running| Status {
        host: None,
        workspace: "/w".into(),
        pid: 1,
        started_at: String::new(),
        clients,
        detached,
        owner,
        session_id: None,
        running,
    };
    assert_eq!(
        quit_decision(Some(&status(1, false, true, false))),
        QuitDecision::Terminate
    );
    assert_eq!(
        quit_decision(Some(&status(2, false, true, false))),
        QuitDecision::Leave
    );
    assert_eq!(
        quit_decision(Some(&status(1, true, true, true))),
        QuitDecision::Ask {
            reason: "a client detached from it earlier".into(),
            running: true
        }
    );
    assert!(matches!(
        quit_decision(Some(&status(1, false, false, false))),
        QuitDecision::Ask { .. }
    ));
    // The daemon could not say (the status failed, or it lost the owner).
    assert!(matches!(quit_decision(None), QuitDecision::Ask { .. }));
}

// `--list`.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_shows_a_live_daemon_and_removes_a_stale_file() {
    use rkat_tui::daemon::endpoint::Endpoint;
    use rkat_tui::daemon::list::{self, Found};
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = owned_daemon(&ws, &llm).await;
    let client = attach(&daemon, None, false).await;
    let rows = list::list(Some(&daemon.endpoint_file)).await;
    assert_eq!(rows.len(), 1);
    let Found::Live(Some(status)) = &rows[0].found else {
        panic!("not live: {:?}", rows[0].found);
    };
    // The probe is not counted.
    assert_eq!(status.clients, 1);
    assert_eq!(status.session_id.as_ref(), Some(&daemon.session));
    let line = list::line(&rows[0], chrono::Utc::now());
    let endpoint = daemon.endpoint();
    assert!(
        line.starts_with(&format!(
            "{}\t{}\tpid {}\tup ",
            std::fs::canonicalize(&ws.root).unwrap().display(),
            endpoint.address,
            endpoint.pid
        )),
        "{line}"
    );
    assert!(line.contains("\t1 client\t"), "{line}");
    assert!(line.ends_with("\tidle"), "{line}");
    assert!(!line.contains(&endpoint.token));
    drop(client);
    daemon.stop().await;

    // A file nothing answers at, of a process that is gone: stale, removed.
    let gone = std::process::Command::new("true").spawn().unwrap();
    let pid = gone.id();
    let _ = { gone }.wait();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let file = ws.sibling("stale").join("mob.endpoint");
    Endpoint {
        address,
        token: "t".into(),
        pid,
        workspace: Some(ws.root.clone()),
        started_at: None,
        realm: None,
    }
    .write(&file)
    .unwrap();
    let rows = list::list(Some(&file)).await;
    assert_eq!(rows[0].found, Found::Stale { removed: true });
    assert!(!file.exists());
    assert!(list::line(&rows[0], chrono::Utc::now()).ends_with("\tstale (file removed)"));
}

/// `--no-attach` while a daemon serves the workspace: the daemon is found
/// and described (workspace, pid, uptime, idle), the notice says what
/// running alongside means; `--no-attach -c` on the daemon's current
/// conversation gets a plain message instead of the engine's "served by
/// another runtime owner".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_attach_beside_a_live_daemon_says_so_and_explains_its_conversation() {
    use rkat_tui::client::alongside;
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("a")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let files = [common::endpoint_file_for(&ws.root)];
    let live = alongside::find(&files).await.expect("the daemon is found");
    let now = chrono::Utc::now();
    let described = live.describe(now);
    let workspace = std::fs::canonicalize(&ws.root).unwrap();
    assert!(
        described.contains(&workspace.display().to_string())
            && described.contains(&format!("pid {}", std::process::id()))
            && described.contains("up ")
            && described.contains("idle"),
        "{described}"
    );
    let notice = live.notice(now);
    assert!(
        notice.contains("no memory search") && notice.contains("two agents"),
        "{notice}"
    );
    assert_eq!(live.session(), Some(&daemon.session));

    // -c in-process on the daemon's conversation.
    let refused = common::Rig::try_start_opts(
        &ws.root,
        &llm,
        rkat_tui::runner::Start::Resume(daemon.session.to_string()),
        common::RigOptions::default(),
    )
    .await
    .err()
    .expect("the engine refuses the daemon's session");
    assert_eq!(
        alongside::served_elsewhere(&refused),
        Some(daemon.session.clone())
    );
    let plain = alongside::explain(&refused, &live).expect("explained");
    assert!(
        plain.to_string().starts_with("that conversation (")
            && plain
                .to_string()
                .contains("is running in the workspace's daemon; attach to it"),
        "{plain}"
    );
    // "Stop the daemon, then run in this process": it ends, and nothing
    // is found any more.
    live.stop(&files).await.unwrap();
    daemon.ended().await;
    assert!(alongside::find(&files).await.is_none());
}

/// The CI failure this guards (`status_reports_the_daemon_its_clients_and_the_owner`
/// saw 2 clients after the third attach completed): once a connection's
/// attach is answered, every other connection's `rkat/status` counts it
/// at once, with no waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answered_attach_is_counted_for_every_other_connection_at_once() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = owned_daemon(&ws, &llm).await;
    let watcher = attach(&daemon, None, false).await;
    let mut held = Vec::new();
    for n in 2..=8 {
        held.push(attach(&daemon, None, false).await);
        assert_eq!(status(&watcher).await.clients, n);
    }
    drop(held);
    drop(watcher);
    daemon.stop().await;
}
