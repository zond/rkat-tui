//! The daemon's lifecycle (P2 of the always-daemon plan, per agent since
//! P5, `docs/mob-split.md`): `rkat/status` reports the clients, the agents,
//! the detach record and the owner (the client that presents the daemon's
//! start token); `rkat/detach` and an agent's last connection ending
//! without retiring it are detaches that keep the agent running;
//! `rkat/retire` retires the caller's agent; `rkat/shutdown` stops the
//! daemon (retiring every agent with `retire`) and removes its endpoint
//! file.

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
use rkat_tui::daemon::protocol::{self, AgentChoice, AgentRow, Status};
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

/// A connection bound to the agent `choice` names.
async fn attach_agent(
    daemon: &Running,
    start_token: Option<&str>,
    choice: AgentChoice,
) -> Connection {
    let (connection, _notes) = Connection::attach_to(
        &daemon.endpoint(),
        Duration::from_secs(5),
        start_token,
        false,
        Some(choice),
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
    // A probe watches: a client binding the agent before the owner's end
    // reached the daemon would keep the agent attended (no detach).
    let watcher = attach(&daemon, None, true).await;
    let seen = status_until(&watcher, |s| s.detached).await;
    assert_eq!(seen.clients, 0);
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
    assert!(
        !file.parent().unwrap().exists(),
        "and its directory, empty now"
    );
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
async fn quit_with_another_client_on_the_agent_leaves_it_to_that_client() {
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
    // The agent still has a client: no detach; the other client, the last
    // one and not the owner, is asked.
    let watcher = attach(&daemon, None, true).await;
    let seen = status_until(&watcher, |s| s.clients == 1).await;
    assert!(!seen.detached, "{seen:?}");
    assert_eq!(seen.agents.len(), 1);
    let QuitDecision::Ask { reason, .. } = decide(&mut other).await else {
        panic!("not asked");
    };
    assert!(reason.contains("did not start"), "{reason}");
    leave(other, Some(Departure::Terminate)).await;
    daemon.ended().await;
}

/// Two clients on two agents: one quits, which retires its agent (gone
/// from `rkat/status`, its conversation still listed and seated again on
/// demand); the other agent still works; the last client's quit follows
/// the P2 rule (the owner, nobody detached: stop).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quitting_one_of_two_agents_retires_it_and_the_last_quit_stops_the_daemon() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("from b"), Step::Text("still here")]);
    let daemon = owned_daemon(&ws, &llm).await;
    let mut a = Client::attach_with(&ws.root, Some(START)).await;
    let mut b = Client::attach_agent(&ws.root, AgentChoice::New).await;
    let a_name = a.agent().await;
    let b_name = b.agent().await;
    assert_ne!(a_name, b_name);
    assert_eq!(
        a_name, daemon.agent,
        "the first client took the start's agent"
    );
    b.send(Effect::StartTurn("b's work".into()));
    assert_eq!(b.turn_finished().await.result.unwrap().text, "from b");
    let b_session = daemon.session_of(&b_name).await;
    // B quits: other clients (A) are attached, so B's agent retires.
    assert_eq!(decide(&mut b).await, QuitDecision::Retire);
    b.send(Effect::Leave(Departure::Retire));
    b.wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.contains("retired")))
        .await;
    b.quit().await;
    let watcher = attach(&daemon, None, true).await;
    let seen = status_until(&watcher, |s| s.clients == 1).await;
    let names: Vec<&str> = seen.agents.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(names, vec![a_name.as_str()], "{seen:?}");
    assert!(!seen.detached, "a retired agent is no detached one");
    // Its conversation is listed, under its agent.
    let listed: serde_json::Value = remote::local(&watcher, protocol::CONVERSATIONS, &json!({}))
        .await
        .unwrap();
    let row = listed["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["session_id"] == json!(b_session.to_string()))
        .cloned()
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(row["agent"], json!(b_name));
    // A still works.
    a.send(Effect::StartTurn("a's work".into()));
    assert_eq!(a.turn_finished().await.result.unwrap().text, "still here");
    // The last client, the owner, nobody detached: /quit stops the daemon.
    assert_eq!(decide(&mut a).await, QuitDecision::Terminate);
    leave(a, Some(Departure::Terminate)).await;
    daemon.ended().await;
}

/// A retired agent is no member: the engine refuses a send to it (no run
/// starts), and its conversation is seated again under its name on demand.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retired_agent_takes_no_input_and_its_conversation_is_seated_again() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("first"), Step::Text("again")]);
    let daemon = owned_daemon(&ws, &llm).await;
    let keeper = attach(&daemon, Some(START), false).await;
    let leaver = attach_agent(&daemon, None, AgentChoice::New).await;
    let name = leaver.agent().to_string();
    assert_ne!(name, keeper.agent());
    leaver
        .call(
            "mob/member_send",
            json!({"mob_id": "tui", "agent_identity": name, "content": "hello"}),
        )
        .await
        .unwrap();
    llm.wait_call(0).await;
    let session = status_until(&leaver, |s| !s.running && s.session_id.is_some())
        .await
        .session_id
        .unwrap();
    let retired: protocol::Retired = remote::local(&leaver, protocol::RETIRE, &json!({}))
        .await
        .unwrap();
    assert_eq!(retired.agent, name);
    assert_eq!(retired.session_id.as_ref(), Some(&session));
    drop(leaver);
    let calls = llm.recorded().len();
    let refused = keeper
        .call(
            "mob/member_send",
            json!({"mob_id": "tui", "agent_identity": name, "content": "wake up"}),
        )
        .await
        .unwrap_err();
    let refused = refused.to_string();
    assert!(
        refused.contains(&name) && refused.to_lowercase().contains("not found"),
        "the engine's not-a-member refusal: {refused}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(llm.recorded().len(), calls, "nothing ran");
    // Seated again on its conversation (the agent's name is the
    // conversation's), which goes on from where it was.
    let back = attach_agent(
        &daemon,
        None,
        AgentChoice::Conversation {
            reference: session.to_string(),
        },
    )
    .await;
    assert_eq!(back.agent(), name);
    assert!(back.seated());
    assert_eq!(remote::session_id(&back).await.unwrap(), session);
    back.call(
        "mob/member_send",
        json!({"mob_id": "tui", "agent_identity": name, "content": "and now"}),
    )
    .await
    .unwrap();
    llm.wait_call(1).await;
    let seen = llm.recorded();
    assert!(seen[1].user().contains("hello"), "the conversation went on");
    drop((back, keeper));
    daemon.stop().await;
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
    // `agents`: (name, clients) of every live agent; this client's is the
    // first.
    let status = |clients, detached, owner, running, agents: &[(&str, usize)]| Status {
        host: None,
        workspace: "/w".into(),
        pid: 1,
        started_at: String::new(),
        clients,
        detached,
        owner,
        session_id: None,
        running,
        agent: agents.first().map(|(name, _)| (*name).to_string()),
        agents: agents
            .iter()
            .map(|(name, clients)| AgentRow {
                name: (*name).to_string(),
                session_id: None,
                running,
                clients: *clients,
                detached,
            })
            .collect(),
    };
    let mine = [("main-aaaa", 1)];
    assert_eq!(
        quit_decision(Some(&status(1, false, true, false, &mine))),
        QuitDecision::Terminate
    );
    // Another client on this agent: leave it to that client.
    assert_eq!(
        quit_decision(Some(&status(2, false, true, false, &[("main-aaaa", 2)]))),
        QuitDecision::Leave
    );
    // Other clients on another agent: this agent retires, the daemon stays.
    assert_eq!(
        quit_decision(Some(&status(
            2,
            false,
            true,
            false,
            &[("main-aaaa", 1), ("main-bbbb", 1)]
        ))),
        QuitDecision::Retire
    );
    assert_eq!(
        quit_decision(Some(&status(1, true, true, true, &mine))),
        QuitDecision::Ask {
            reason: "an agent runs here without a client (detached earlier)".into(),
            running: true
        }
    );
    // The last client, with another agent left running without one: ask.
    assert!(matches!(
        quit_decision(Some(&status(
            1,
            false,
            true,
            false,
            &[("main-aaaa", 1), ("main-bbbb", 0)]
        ))),
        QuitDecision::Ask { .. }
    ));
    assert!(matches!(
        quit_decision(Some(&status(1, false, false, false, &mine))),
        QuitDecision::Ask { .. }
    ));
    // A daemon of before P5 (no agent list): another client means leave.
    assert_eq!(
        quit_decision(Some(&status(2, false, true, false, &[]))),
        QuitDecision::Leave
    );
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
    assert!(line.ends_with("\t1 client\t1 agent"), "{line}");
    assert!(!line.contains(&endpoint.token));
    // The agent under it.
    let lines = list::lines(&rows[0], chrono::Utc::now());
    assert_eq!(
        lines[1],
        format!("\t{}\tidle\t{}\t1 client", daemon.agent, daemon.session)
    );
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

/// The last connection state `client` was shown.
fn connection_state(client: &Client) -> Option<rkat_tui::ui::Connection> {
    client.seen.iter().rev().find_map(|msg| match msg {
        Msg::Status(rkat_tui::ui::StatusUpdate::Connection(state)) => Some(state.clone()),
        _ => None,
    })
}

/// The finding this guards (second review, item 2): a failed first attach
/// set the status to `detached`, said why once in a notice that scrolled
/// away and never tried again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_that_exits_at_start_is_retried_reported_and_started_by_hand() {
    use std::os::unix::fs::PermissionsExt as _;
    let ws = Workspace::new();
    let endpoint_file = common::endpoint_file_for(&ws.root);
    let bin = ws.root.parent().unwrap().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let starts = bin.join("starts");
    let program = bin.join("rkat-tui-mob");
    std::fs::write(
        &program,
        format!("#!/bin/sh\necho start >> '{}'\nexit 3\n", starts.display()),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = bin.join("daemon.log");
    let args = args_for(&ws.root);
    let mut config = rkat_tui::client::configure(&args, rkat_tui::cli::AttachTarget::Existing)
        .await
        .unwrap();
    config.endpoint_files = vec![endpoint_file.clone()];
    config.spawn = Some(rkat_tui::client::runner::SpawnPlan {
        program,
        argv: Vec::new(),
        log: log.clone(),
    });
    config.lister = common::no_lists();
    let mut client = Client {
        harness: rkat_tui::client::testing::spawn_attach_runner(config),
        seen: Vec::new(),
    };
    // Retried with a backoff, then given up on with the reason and the log.
    let given_up = |msg: &Msg| {
        matches!(
            msg,
            Msg::Status(rkat_tui::ui::StatusUpdate::Connection(
                rkat_tui::ui::Connection::Failed { retry_at: None, .. }
            ))
        )
    };
    client.wait_for(given_up).await;
    let retried = client
        .seen
        .iter()
        .filter(|msg| {
            matches!(
                msg,
                Msg::Status(rkat_tui::ui::StatusUpdate::Connection(
                    rkat_tui::ui::Connection::Failed {
                        retry_at: Some(_),
                        ..
                    }
                ))
            )
        })
        .count();
    assert_eq!(retried, 3, "three retries before giving up");
    assert_eq!(
        std::fs::read_to_string(&starts).unwrap().lines().count(),
        4,
        "the first start and three restarts"
    );
    let Some(rkat_tui::ui::Connection::Failed {
        reason, log: shown, ..
    }) = connection_state(&client)
    else {
        panic!("{:?}", connection_state(&client));
    };
    assert!(
        reason.contains("exited") && reason.contains('3'),
        "{reason}"
    );
    assert_eq!(shown.as_deref(), Some(log.as_path()));
    // Once a daemon serves, /daemon start attaches.
    let llm = ScriptedLlmClient::new(vec![Step::Text("served")]);
    let daemon = Running::start_with(&args, Running::options(&ws.root, &llm)).await;
    client.send(Effect::DaemonStart);
    client.attached().await;
    client.send(Effect::StartTurn("hello".into()));
    assert_eq!(client.turn_finished().await.result.unwrap().text, "served");
    client.quit().await;
    daemon.stop().await;
}

/// A client attached to a daemon of another build says so (the user
/// rebuilt the binaries under a running daemon).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_of_another_build_is_announced() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start_with(
        &args_for(&ws.root),
        Running::options(&ws.root, &llm).with_build("0.0.0+elsewhere"),
    )
    .await;
    let mut client = Client::attach(&ws.root).await;
    client
        .saw(|msg| {
            matches!(msg, Msg::Notice { text, .. }
                if text.contains("the daemon runs build 0.0.0+elsewhere")
                    && text.contains(rkat_tui::BUILD))
        })
        .await;
    assert_eq!(
        connection_state(&client),
        Some(rkat_tui::ui::Connection::Attached {
            address: daemon.endpoint().address.to_string(),
            daemon_build: Some("0.0.0+elsewhere".into()),
        })
    );
    client.quit().await;
    daemon.stop().await;
}
