//! Wires between agents of different workspaces (`/wire`): two daemons,
//! each in its own process (two cannot share one: the engine's in-process
//! comms registry is process-global). The test reruns itself as the other
//! workspace's side ([`OTHER_SIDE`]), and the two work in step through
//! marker files; each side asserts on its own daemon and model.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::common::{self, ScriptedLlmClient, Step, Workspace, args_for, endpoint_file_for};
use crate::rig::{self, Client, Running};
use parking_lot::Mutex;
use rkat_tui::client::remote;
use rkat_tui::client::transport::Connection;
use rkat_tui::daemon::protocol::{self, WireAgentParams, WireCard, WireParams, method};
use rkat_tui::ui::{Effect, Msg, NoticeLevel, PickerKind};
use serde_json::json;

/// Set in the rerun: which side it plays.
const OTHER_SIDE: &str = "RKAT_TUI_WIRE_SIDE";
/// Set in the rerun: the marker directory.
const MARKERS: &str = "RKAT_TUI_WIRE_MARKERS";

/// The marker files the two sides work in step through.
struct Markers(PathBuf);

impl Markers {
    fn set(&self, name: &str, body: &str) {
        let tmp = self.0.join(format!("{name}.tmp"));
        std::fs::write(&tmp, body).unwrap();
        std::fs::rename(&tmp, self.0.join(name)).unwrap();
    }

    async fn wait(&self, name: &str) -> String {
        let path = self.0.join(name);
        let deadline = Instant::now() + Duration::from_mins(2);
        while Instant::now() < deadline {
            if let Ok(body) = std::fs::read_to_string(&path) {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the other side never set {name}");
    }
}

/// An agent's script: a prompt `SEND <words>` sends `<tag>: <words>` to
/// the peer id `to` holds; after the tool's result it says `sent`;
/// anything else (a peer message) it answers `woke`.
fn script(to: &Arc<Mutex<String>>, tag: &'static str) -> Vec<Step> {
    let to = Arc::clone(to);
    let step = Step::Dynamic(Arc::new(
        move |messages: &[meerkat_core::Message]| match messages.last() {
            Some(meerkat_core::Message::ToolResults { .. }) => Step::Text("sent"),
            Some(meerkat_core::Message::User(user)) if user.text_content().starts_with("SEND") => {
                let words = user
                    .text_content()
                    .trim_start_matches("SEND")
                    .trim()
                    .to_string();
                Step::Tool {
                    id: "m1",
                    name: "send_message",
                    args: json!({"peer_id": to.lock().clone(), "body": format!("{tag}: {words}"), "handling_mode": "queue"}),
                }
            }
            _ => Step::Text("woke"),
        },
    ));
    vec![step; 80]
}

/// The text of the last tool result `llm` was shown.
fn last_result(llm: &ScriptedLlmClient) -> String {
    llm.requests()
        .iter()
        .rev()
        .find_map(|messages| {
            messages.iter().rev().find_map(|m| match m {
                meerkat_core::Message::ToolResults { results, .. } => Some(
                    results
                        .iter()
                        .map(meerkat_core::ToolResult::text_content)
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
        })
        .unwrap_or_default()
}

/// Every peer message `llm` was shown (the engine's rendering).
fn peer_messages(llm: &ScriptedLlmClient) -> Vec<String> {
    let mut out = Vec::new();
    for messages in llm.requests() {
        for m in messages {
            if let meerkat_core::Message::SystemNotice(notice) = &m {
                for block in &notice.blocks {
                    let text = format!("{block:?}");
                    if let Some(at) = text.find("Peer message from ") {
                        let line = text[at..]
                            .split('"')
                            .next()
                            .unwrap_or("")
                            .replace("\\n", "\n");
                        if !out.contains(&line) {
                            out.push(line);
                        }
                    }
                }
            }
        }
    }
    out
}

/// Everything the last request of `llm` held, debug-printed (the
/// daemon's notes are in it).
fn context(llm: &ScriptedLlmClient) -> String {
    format!("{:?}", llm.recorded().last().unwrap().messages)
}

/// Wait until `llm` was shown a peer message holding `text`.
async fn shown(llm: &ScriptedLlmClient, text: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let seen = peer_messages(llm);
        if seen.iter().any(|m| m.contains(text)) {
            return seen;
        }
        assert!(
            Instant::now() < deadline,
            "never shown {text:?}; shown: {seen:#?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Start a turn of `daemon`'s agent with `prompt` and wait for it to end.
async fn turn(daemon: &Running, prompt: &str) {
    let connection = daemon.connect_bound().await;
    connection
        .call(
            "mob/turn_start",
            json!({"mob_id": "tui", "agent_identity": daemon.agent, "prompt": prompt}),
        )
        .await
        .unwrap();
    daemon.idle(&daemon.agent).await;
}

/// The card of `daemon`'s agent, as its own client asks for it.
async fn card(daemon: &Running) -> WireCard {
    let connection = daemon.connect_bound().await;
    remote::call::<method::Card>(&connection, &WireAgentParams::default())
        .await
        .unwrap()
}

/// A workspace named `name` in a fresh temporary directory.
fn workspace(name: &str) -> (Workspace, PathBuf) {
    let ws = Workspace::new();
    let root = ws.sibling(name);
    std::fs::create_dir_all(&root).unwrap();
    (ws, root)
}

/// A client of `root`'s daemon whose `/wire` looks at `others`.
async fn client(root: &Path, others: &[PathBuf]) -> Client {
    let mut config = rig::client_config(
        &args_for(root),
        None,
        common::no_lists(),
        &endpoint_file_for(root),
        None,
    )
    .await;
    config.wire_files = Some(others.to_vec());
    Client::attach_with_config(config).await
}

/// The text of the next notice holding `text`.
async fn notice(client: &mut Client, text: &str) -> (NoticeLevel, String) {
    match client
        .wait_for(|msg| matches!(msg, Msg::Notice { text: t, .. } if t.contains(text)))
        .await
    {
        Msg::Notice { level, text } => (level, text),
        _ => unreachable!(),
    }
}

/// The agent's cross-workspace wires as its daemon lists them.
async fn wires_of(daemon: &Running) -> Vec<String> {
    let status = daemon.status().await;
    status
        .agents
        .iter()
        .find(|row| row.name == daemon.agent)
        .map(|row| row.wires.iter().map(protocol::WireRow::label).collect())
        .unwrap_or_default()
}

/// The other workspace's side (`beta`): its daemon, its model, its
/// sends; restarted when the first side says so.
async fn beta(markers: Markers) {
    let (_ws, root) = workspace("beta");
    let to_alpha = Arc::new(Mutex::new(String::new()));
    let llm = ScriptedLlmClient::new(script(&to_alpha, "from B"));
    let b = Running::start(&root, &llm).await;
    markers.set(
        "b.ready",
        &format!("{}\n{}", b.endpoint_file.display(), b.agent),
    );
    // The first side wires both ways from its client.
    *to_alpha.lock() = markers.wait("a.peer").await;
    let a_agent = markers.wait("a.agent").await;
    let from_alpha = format!("Peer message from alpha/{a_agent}:\nfrom A: hello");
    shown(&llm, &from_alpha).await;
    b.idle(&b.agent).await;
    let system = context(&llm);
    assert!(
        system.contains(&format!("You are now wired to `alpha/{a_agent}`")),
        "{system}"
    );
    assert_eq!(wires_of(&b).await, vec![format!("alpha/{a_agent}")]);
    turn(&b, "SEND reply").await;
    assert!(
        last_result(&llm).contains("\"acked\""),
        "{}",
        last_result(&llm)
    );
    markers.set("b.replied", "");

    // The first side's agent switched conversation: a new key, which its
    // client sent here.
    *to_alpha.lock() = markers.wait("a.peer2").await;
    turn(&b, "SEND afternew").await;
    assert!(
        last_result(&llm).contains("\"acked\""),
        "a send after the other agent's /new: {}",
        last_result(&llm)
    );
    markers.set("b.afternew", "");

    // Down, then up again on the same socket, its agent idle with no
    // comms runtime until a connection wakes it.
    markers.wait("restart_b").await;
    b.stop().await;
    markers.set("b.down", "");
    markers.wait("a.sent_down").await;
    let llm = ScriptedLlmClient::new(script(&to_alpha, "from B2"));
    let b = Running::start(&root, &llm).await;
    assert!(!b.created, "the agent is restored");
    markers.set("b.up", "");
    shown(&llm, "from A: afterrestart").await;
    b.idle(&b.agent).await;
    turn(&b, "SEND back").await;
    assert!(
        last_result(&llm).contains("\"acked\""),
        "{}",
        last_result(&llm)
    );
    markers.set("b.back", "");

    // The first side restarted.
    markers.wait("a.up").await;
    turn(&b, "SEND toa").await;
    assert!(
        last_result(&llm).contains("\"acked\""),
        "{}",
        last_result(&llm)
    );
    markers.set("b.toa", "");

    // Unwired from the first side's client: both sides.
    markers.wait("unwired").await;
    assert_eq!(wires_of(&b).await, Vec::<String>::new());
    turn(&b, "SEND afterunwire").await;
    let result = last_result(&llm);
    assert!(
        !result.contains("\"acked\""),
        "a send after the unwire: {result}"
    );
    markers.set("b.done", "");
    b.stop().await;
}

/// `/wire` from a client: the picker lists the other workspace's agent;
/// Enter wires both ways (each told, with the other's
/// `<workspace>/<agent>` name); messages go both ways and wake an idle
/// agent; after a `/new` (a new key) the client refreshes the other side;
/// the other daemon down fails a send at once, and once it is back (its
/// agent restored, idle) the wire works again without re-wiring, from both
/// sides; a restart of this side too; `/unwire` removes both sides.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::disallowed_methods)] // test-only process launch: the other side
async fn wired_agents_of_two_workspaces_message_both_ways_across_restarts() {
    if std::env::var(OTHER_SIDE).as_deref() == Ok("beta") {
        beta(Markers(PathBuf::from(std::env::var(MARKERS).unwrap()))).await;
        return;
    }
    let (ws, root) = workspace("alpha");
    let markers_dir = ws.sibling("markers");
    std::fs::create_dir_all(&markers_dir).unwrap();
    let mut other = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cross_workspace::wired_agents_of_two_workspaces_message_both_ways_across_restarts",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(OTHER_SIDE, "beta")
        .env(MARKERS, &markers_dir)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let markers = Markers(markers_dir);
    let ready = markers.wait("b.ready").await;
    let (b_file, b_agent) = ready.split_once('\n').unwrap();
    let b_file = PathBuf::from(b_file);
    let b_label = format!("beta/{b_agent}");

    let to_beta = Arc::new(Mutex::new(String::new()));
    let llm = ScriptedLlmClient::new(script(&to_beta, "from A"));
    let a = Running::start(&root, &llm).await;
    let mut c = client(&root, std::slice::from_ref(&b_file)).await;

    // The picker: the other workspace's one agent, idle, no client.
    c.send(Effect::Wire(None));
    let picker = match c
        .wait_for(
            |msg| matches!(msg, Msg::ShowPicker { picker, .. } if picker.kind == PickerKind::Wire),
        )
        .await
    {
        Msg::ShowPicker { picker, .. } => picker,
        _ => unreachable!(),
    };
    assert_eq!(picker.rows.len(), 1, "{picker:?}");
    assert_eq!(picker.rows[0].text, format!("{b_label}  idle  no client"));
    let b_root = PathBuf::from(picker.rows[0].note.clone().unwrap());
    assert!(b_root.ends_with("beta"), "{b_root:?}");
    c.send(Effect::WireTo {
        workspace: b_root.clone(),
        agent: b_agent.to_string(),
    });
    let a_label = format!("alpha/{}", a.agent);
    let (level, text) = notice(&mut c, "wired ").await;
    assert_eq!(level, NoticeLevel::Info, "{text}");
    assert!(
        text.contains(&format!("wired {a_label} and {b_label}"))
            && text.contains(&format!("/unwire {b_label} removes it")),
        "{text}"
    );
    // The picker stays, its row now marked wired (review 5, P1).
    let refreshed = c
        .wait_for(|msg| {
            matches!(msg, Msg::ShowPicker { picker, refresh: true } if picker.kind == PickerKind::Wire)
        })
        .await;
    assert!(
        matches!(&refreshed, Msg::ShowPicker { picker, .. }
            if picker.rows.len() == 1 && picker.rows[0].text.ends_with("· wired")),
        "{refreshed:?}"
    );
    // Again: nothing changes, and the user is told so.
    c.send(Effect::Wire(Some("beta".into())));
    let (_, again) = notice(&mut c, "wired already").await;
    assert!(
        again.contains(&format!("{a_label} and {b_label} are wired already")),
        "{again}"
    );
    assert_eq!(wires_of(&a).await, vec![b_label.clone()]);

    // A to B.
    let b_card = {
        let endpoint = rkat_tui::daemon::endpoint::Endpoint::read(&b_file).unwrap();
        let (probe, _) = Connection::attach_as(&endpoint, Duration::from_secs(5), None, true)
            .await
            .unwrap();
        remote::call::<method::Card>(
            &probe,
            &WireAgentParams {
                agent: Some(b_agent.to_string()),
            },
        )
        .await
        .unwrap()
    };
    *to_beta.lock() = b_card.peer_id.clone();
    markers.set("a.peer", &card(&a).await.peer_id);
    markers.set("a.agent", &a.agent);
    turn(&a, "SEND hello").await;
    assert!(
        last_result(&llm).contains("\"acked\""),
        "{}",
        last_result(&llm)
    );
    let system = context(&llm);
    assert!(
        system.contains(&format!(
            "You are now wired to `{b_label}`, an agent of another workspace"
        )),
        "{system}"
    );
    // B to A: A's agent wakes for it, the sender named by its workspace.
    markers.wait("b.replied").await;
    shown(
        &llm,
        &format!("Peer message from {b_label}:\nfrom B: reply"),
    )
    .await;
    a.idle(&a.agent).await;
    // This client labels it with the other workspace's name.
    let label = b_label.clone();
    c.wait_for(move |msg| matches!(msg, Msg::Peer { from, .. } if *from == label))
        .await;
    // The run it woke is this client's turn too: `/new` is refused until
    // the client has seen it end (the daemon's idle status can come first).
    c.wait_for(|msg| matches!(msg, Msg::TurnFinished(_))).await;

    // A new conversation: a new key; the client tells the other side.
    let before = card(&a).await;
    c.send(Effect::NewSession);
    notice(&mut c, "a new conversation").await;
    let after = within_secs(30, async {
        loop {
            let after = card(&a).await;
            if after.pubkey != before.pubkey {
                return after;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    // The refresh is the client's own task: wait for the other side to
    // hold the new key (its send below fails until then).
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !c.notices()
            .contains("does not know this agent's new conversation"),
        "{}",
        c.notices()
    );
    markers.set("a.peer2", &after.peer_id);
    markers.wait("b.afternew").await;
    shown(&llm, "from B: afternew").await;
    a.idle(&a.agent).await;
    assert_eq!(
        wires_of(&a).await,
        vec![b_label.clone()],
        "the wire after /new"
    );

    // B down: a send fails at once and says why.
    markers.set("restart_b", "");
    markers.wait("b.down").await;
    let started = Instant::now();
    turn(&a, "SEND whiledown").await;
    let down = last_result(&llm);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(
        down.contains(&format!(
            "`{b_label}` cannot get messages now: the rkat-tui daemon of its workspace ({}) is not running",
            b_root.display()
        )) && down.contains("send it again"),
        "{down}"
    );
    markers.set("a.sent_down", "");
    // B up again: the wire works without re-wiring.
    markers.wait("b.up").await;
    turn(&a, "SEND afterrestart").await;
    assert!(
        last_result(&llm).contains("\"acked\""),
        "{}",
        last_result(&llm)
    );
    markers.wait("b.back").await;
    shown(&llm, "from B2: back").await;
    a.idle(&a.agent).await;

    // A restarts.
    c.quit().await;
    a.stop().await;
    let llm = ScriptedLlmClient::new(script(&to_beta, "from A2"));
    let a = Running::start(&root, &llm).await;
    assert!(!a.created, "the agent is restored");
    assert_eq!(
        wires_of(&a).await,
        vec![b_label.clone()],
        "the wire after a restart"
    );
    markers.set("a.up", "");
    markers.wait("b.toa").await;
    shown(&llm, "from B2: toa").await;
    a.idle(&a.agent).await;

    // `/unwire` from its picker: both sides, and the picker stays with
    // the wire gone from it (review 5, P1).
    let mut c = client(&root, std::slice::from_ref(&b_file)).await;
    c.send(Effect::UnwireFrom {
        workspace: b_root.clone(),
        agent: b_agent.to_string(),
    });
    let (_, text) = notice(&mut c, "unwired").await;
    assert_eq!(text, format!("unwired {b_label} on both sides"));
    let refreshed = c
        .wait_for(|msg| {
            matches!(msg, Msg::ShowPicker { picker, refresh: true } if picker.kind == PickerKind::Unwire)
        })
        .await;
    assert!(
        matches!(&refreshed, Msg::ShowPicker { picker, .. } if picker.rows.is_empty()),
        "{refreshed:?}"
    );
    assert_eq!(wires_of(&a).await, Vec::<String>::new());
    let system_before = llm.recorded().len();
    markers.set("unwired", "");
    markers.wait("b.done").await;
    assert_eq!(
        llm.recorded().len(),
        system_before,
        "a message after the unwire reached the agent"
    );
    c.quit().await;
    a.stop().await;
    let status = within_secs(120, other.wait()).await.unwrap();
    assert!(status.success(), "the other side failed: {status}");
}

async fn within_secs<F: std::future::Future>(secs: u64, future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(secs), future)
        .await
        .expect("timed out")
}

/// A card of an agent of another workspace that does not run: a key of
/// its own, a socket nothing listens on.
fn stranger(workspace: &Path, agent: &str) -> (WireCard, meerkat_comms::Keypair) {
    let key = meerkat_comms::Keypair::generate();
    let public = key.public_key();
    (
        WireCard {
            workspace: workspace.to_path_buf(),
            agent: agent.to_string(),
            peer_id: public.to_peer_id().to_string(),
            pubkey: public.to_pubkey_string(),
            address: format!("uds://{}", workspace.join("nothing.sock").display()),
        },
        key,
    )
}

/// The daemon side alone: wiring twice to one key is idempotent (one
/// note), a send to a wired peer is tier 1 (no review; one to an unknown
/// peer id is reviewed), the roster names the wire, `rkat/status` lists
/// it, unwiring removes it (twice: nothing to remove), and the refusals:
/// a card of this workspace, a card whose peer id is not its key's, a
/// bound caller naming another agent, a probe naming none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_wires_unwires_and_refuses() {
    let (ws, root) = workspace("alpha");
    let other = ws.sibling("gamma");
    let (gamma, _) = stranger(&other, "main-gggg");
    let unknown = meerkat_comms::Keypair::generate().public_key().to_peer_id();
    let classifier = rig::ScriptedClassifier::allowing(vec![]);
    let llm = ScriptedLlmClient::new(vec![
        Step::Text("first"),
        Step::Tool {
            id: "s1",
            name: "send_message",
            args: json!({"peer_id": gamma.peer_id, "body": "hi", "handling_mode": "queue"}),
        },
        Step::Tool {
            id: "s2",
            name: "send_message",
            args: json!({"peer_id": unknown.to_string(), "body": "hi", "handling_mode": "queue"}),
        },
        Step::Text("tried"),
        // Plan mode: the same send to the wired peer is put to the user.
        Step::Tool {
            id: "s3",
            name: "send_message",
            args: json!({"peer_id": gamma.peer_id, "body": "plan", "handling_mode": "queue"}),
        },
        Step::Text("asked"),
    ]);
    let setup = rig::Setup {
        escalation_timeout: Some(Duration::from_millis(300)),
        ..rig::Setup::default()
    }
    .classifier(Arc::clone(&classifier) as _);
    let a = Running::start_with(&args_for(&root), setup.options(&root, &llm)).await;
    turn(&a, "hello").await;
    let bound = a.connect_bound().await;
    let wire = |card: WireCard, agent: Option<String>| {
        let bound = &bound;
        async move { remote::call::<method::Wire>(bound, &WireParams { agent, card }).await }
    };
    let first = wire(gamma.clone(), None).await.unwrap();
    assert_eq!(
        (first.label.as_str(), first.already),
        ("gamma/main-gggg", false)
    );
    let second = wire(gamma.clone(), None).await.unwrap();
    assert!(second.already, "{second:?}");
    assert_eq!(wires_of(&a).await, vec!["gamma/main-gggg".to_string()]);

    // Refusals.
    let (own, _) = stranger(&root, "main-zzzz");
    let err = wire(own, None).await.unwrap_err().to_string();
    assert!(
        err.contains(&protocol::WIRE_REFUSED.to_string())
            && err.contains("works in this workspace: the agents of one workspace are wired to each other already"),
        "{err}"
    );
    // A card whose peer id is not its key's (review 5, W1: the gate
    // compares the stored id, so it is checked once, here).
    let (mut forged, _) = stranger(&other, "main-ffff");
    forged.peer_id = unknown.to_string();
    let err = wire(forged, None).await.unwrap_err().to_string();
    assert!(err.contains("is not its key's"), "{err}");
    assert_eq!(wires_of(&a).await, vec!["gamma/main-gggg".to_string()]);
    let err = wire(gamma.clone(), Some("main-other".into()))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(&protocol::NOT_YOURS.to_string()), "{err}");
    let probe = a.connect().await;
    let err = remote::call::<method::Card>(&probe, &WireAgentParams::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("names the agent"), "{err}");
    let named = remote::call::<method::Card>(
        &probe,
        &WireAgentParams {
            agent: Some(a.agent.clone()),
        },
    )
    .await
    .unwrap();
    assert!(
        named.address.starts_with("uds://") && named.address.ends_with(".sock"),
        "{named:?}"
    );

    // The next turn: the note (once), the roster line, the sends.
    turn(&a, "go").await;
    let system = context(&llm);
    assert_eq!(
        system
            .matches("You are now wired to `gamma/main-gggg`")
            .count(),
        1,
        "{system}"
    );
    let reviewed = classifier.tools();
    assert_eq!(
        reviewed.iter().filter(|t| *t == "send_message").count(),
        1,
        "only the send to the unknown peer is reviewed: {reviewed:?}"
    );

    // Review 5, AP2 (owner decision): in plan mode a message to an agent
    // of another workspace is put to the user (nobody answers here, so it
    // times out as a denial); auto mode above sent it without review.
    // The peer's socket must look alive, or the send is refused as
    // unreachable first.
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("nothing.sock"), b"").unwrap();
    remote::call::<method::SetMode>(
        &bound,
        &protocol::SetModeParams {
            mode: rkat_tui::approval::mode::PermissionMode::Plan,
        },
    )
    .await
    .unwrap();
    turn(&a, "plan").await;
    let asked = last_result(&llm);
    assert!(
        asked.contains("did not answer the approval request"),
        "{asked}"
    );
    assert_eq!(
        classifier
            .tools()
            .iter()
            .filter(|t| *t == "send_message")
            .count(),
        1,
        "the plan-mode ask is the user's, not the classifier's"
    );

    // Unwire, twice.
    let params = protocol::UnwireParams {
        agent: None,
        workspace: other.clone(),
        peer: "main-gggg".into(),
    };
    let unwire = || remote::call::<method::Unwire>(&bound, &params);
    assert!(unwire().await.unwrap().removed);
    assert!(!unwire().await.unwrap().removed);
    assert_eq!(wires_of(&a).await, Vec::<String>::new());

    // A wire whose card cannot be saved is undone, so its error says what
    // is (review 5, W2): the records file is a directory now.
    let records = common::realm_of(&root).join(rkat_tui::daemon::wires::RECORDS_FILE);
    let _ = std::fs::remove_file(&records);
    std::fs::create_dir(&records).unwrap();
    let (delta, _) = stranger(&ws.sibling("delta"), "main-dddd");
    let err = wire(delta, None).await.unwrap_err().to_string();
    assert!(err.contains("cannot save the wires"), "{err}");
    let members = a
        .state()
        .handle_for(&meerkat_mob::MobId::from("tui"))
        .await
        .unwrap()
        .list_members()
        .await;
    let label = meerkat_mob::AgentIdentity::from("delta/main-dddd");
    assert!(
        members
            .iter()
            .all(|member| !member.wired_to.contains(&label)),
        "the unsaved wire was undone"
    );
    assert_eq!(wires_of(&a).await, Vec::<String>::new());
    std::fs::remove_dir(&records).unwrap();
    drop(bound);
    drop(probe);
    a.stop().await;
}

/// A socket path longer than `sun_path` is refused at the agent's bind,
/// and `/wire` says which path and the limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_socket_path_too_long_is_refused_naming_it() {
    let (ws, root) = workspace("alpha");
    let long = ws.sibling(&"c".repeat(80));
    let llm = ScriptedLlmClient::new(vec![]);
    let args = common::args_with(&root, &["--config-dir", long.to_str().unwrap()]);
    let a = Running::start_with(&args, Running::options(&root, &llm)).await;
    let bound = a.connect_bound().await;
    let err = remote::call::<method::Card>(&bound, &WireAgentParams::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("wire socket")
            && err.contains(&format!("{}.sock", a.agent))
            && err.contains(&format!(
                "the {} a Unix socket path may have",
                rkat_tui::daemon::wires::MAX_SOCKET_PATH
            )),
        "{err}"
    );
    drop(bound);
    a.stop().await;
}

/// Envelopes written to an agent's socket by hand are not delivered: one
/// with no signature or a wrong one claiming a wired peer's key, and one
/// signed by a key the agent does not trust. None is acknowledged, and
/// the agent is never woken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_envelopes_on_the_socket_are_not_delivered() {
    use futures::{SinkExt, StreamExt};
    let (ws, root) = workspace("alpha");
    let (gamma, _) = stranger(&ws.sibling("gamma"), "main-gggg");
    let llm = ScriptedLlmClient::new(vec![]);
    let a = Running::start(&root, &llm).await;
    let bound = a.connect_bound().await;
    remote::call::<method::Wire>(
        &bound,
        &WireParams {
            agent: None,
            card: gamma.clone(),
        },
    )
    .await
    .unwrap();
    let mine = remote::call::<method::Card>(&bound, &WireAgentParams::default())
        .await
        .unwrap();
    let socket = PathBuf::from(mine.address.trim_start_matches("uds://"));
    let to = meerkat_comms::PubKey::from_pubkey_string(&mine.pubkey).unwrap();
    let trusted = meerkat_comms::PubKey::from_pubkey_string(&gamma.pubkey).unwrap();
    let stranger_key = meerkat_comms::Keypair::generate();
    let envelope = |id: u128, from, body: &str| meerkat_comms::Envelope {
        id: uuid::Uuid::from_u128(id),
        from,
        to,
        kind: meerkat_comms::MessageKind::Message {
            body: body.to_string(),
            blocks: None,
            content_taint: None,
            handling_mode: None,
            objective_id: None,
            in_reply_to: None,
        },
        sig: meerkat_comms::Signature::new([0u8; 64]),
    };
    let unsigned = envelope(1, trusted, "forged unsigned");
    let mut wrong = envelope(2, trusted, "forged wrongly signed");
    wrong.sign(&stranger_key);
    wrong.from = trusted;
    let mut untrusted = envelope(3, stranger_key.public_key(), "forged untrusted");
    untrusted.sign(&stranger_key);
    for (what, env) in [
        ("unsigned", unsigned),
        ("wrongly signed", wrong),
        ("untrusted", untrusted),
    ] {
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let mut framed =
            tokio_util::codec::Framed::new(stream, meerkat_comms::TransportCodec::new(1 << 20));
        framed
            .send(meerkat_comms::EnvelopeFrame::from_envelope(env))
            .await
            .unwrap();
        let answer = tokio::time::timeout(Duration::from_secs(3), framed.next()).await;
        assert!(
            matches!(answer, Ok(None | Some(Err(_)))),
            "the {what} envelope was answered: {answer:?}"
        );
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        llm.requests().is_empty(),
        "a forged envelope woke the agent"
    );
    // The socket is the owner's alone.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        let dir = std::fs::metadata(socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!((mode, dir), (0o600, 0o700));
    }
    drop(bound);
    a.stop().await;
    assert!(
        !socket.exists(),
        "the socket is removed at the daemon's stop"
    );
}

/// `/wire` with no other daemon running says how to get one; a target in
/// this workspace is refused (its agents are wired already).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wire_without_another_daemon_says_what_to_do() {
    let (_ws, root) = workspace("alpha");
    let llm = ScriptedLlmClient::new(vec![]);
    let a = Running::start(&root, &llm).await;
    let mut c = client(&root, &[]).await;
    c.send(Effect::Wire(None));
    let picker = match c
        .wait_for(
            |msg| matches!(msg, Msg::ShowPicker { picker, .. } if picker.kind == PickerKind::Wire),
        )
        .await
    {
        Msg::ShowPicker { picker, .. } => picker,
        _ => unreachable!(),
    };
    assert!(picker.rows.is_empty());
    assert!(
        picker.empty.contains("no other workspace's daemon runs"),
        "{}",
        picker.empty
    );
    c.send(Effect::Wire(Some("beta".into())));
    let (_, text) = notice(&mut c, "nothing was wired").await;
    assert!(
        text.contains("start rkat-tui in the other workspace first"),
        "{text}"
    );
    c.send(Effect::Wire(Some(format!("alpha/{}", a.agent))));
    let (_, text) = notice(&mut c, "this workspace's").await;
    assert!(text.contains("wired to each other already"), "{text}");
    c.send(Effect::Unwire(None));
    let picker = match c
        .wait_for(|msg| matches!(msg, Msg::ShowPicker { picker, .. } if picker.kind == PickerKind::Unwire))
        .await
    {
        Msg::ShowPicker { picker, .. } => picker,
        _ => unreachable!(),
    };
    assert!(
        picker.empty.contains("no wires to other workspaces"),
        "{}",
        picker.empty
    );
    c.quit().await;
    a.stop().await;
}
