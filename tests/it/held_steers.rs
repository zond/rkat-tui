//! The user's messages typed while one of theirs waits in the engine: the
//! first goes at once, the ones after it are held in the daemon and go as
//! one steer when the first is written into a request
//! (`rkat_tui::daemon::activity::steers`). Through the real daemon with a
//! scripted model whose requests are held, and an attached client.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, user_texts};
use rig::Attached;
use rkat_tui::ui::delivery::PendingId;
use rkat_tui::ui::{Effect, Msg};
use serde_json::json;
use tokio::sync::Notify;

fn steer(id: u64, text: &str) -> Effect {
    Effect::Steer {
        id: PendingId(id),
        text: text.into(),
    }
}

fn contacts(id: &'static str) -> Step {
    Step::Tool {
        id,
        name: "contacts",
        args: json!({}),
    }
}

/// The user texts request `at` added after request `at - 1`'s.
fn added(llm: &ScriptedLlmClient, at: usize) -> Vec<String> {
    let requests = llm.requests();
    let before = user_texts(&requests[at - 1]).len();
    user_texts(&requests[at])[before..].to_vec()
}

/// Whether request `at` holds `text` anywhere.
fn carried(llm: &ScriptedLlmClient, at: usize, text: &str) -> bool {
    llm.requests()
        .get(at)
        .is_some_and(|messages| format!("{messages:?}").contains(text))
}

/// Type `text` as message `id` and wait for its acknowledgement.
async fn typed(rig: &mut Attached, id: u64, text: &str) {
    rig.send(steer(id, text));
    rig.saw(
        move |msg| matches!(msg, Msg::SteerAck { id: PendingId(i), result: Ok(()) } if *i == id),
    )
    .await;
}

/// The held messages go when the first is written into request 1, and
/// ride request 2 even when request 1 answers at once with a tool that
/// runs at once: nothing of the rig holds request 1 for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_messages_ride_the_request_after_an_instant_one() {
    let ws = Workspace::new();
    let first = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&first), Box::new(contacts("c0"))),
        contacts("c1"),
        contacts("c2"),
        Step::Text("done"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("go".into()));
    llm.wait_call(0).await;
    typed(&mut rig, 1, "FAST-ONE").await;
    typed(&mut rig, 2, "FAST-TWO").await;
    typed(&mut rig, 3, "FAST-THREE").await;
    first.notify_one();
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "done");
    let requests = llm.requests();
    let at = |text: &str| {
        requests
            .iter()
            .position(|messages| format!("{messages:?}").contains(text))
    };
    assert_eq!(
        (at("FAST-ONE"), at("FAST-TWO"), at("FAST-THREE")),
        (Some(1), Some(2), Some(2)),
        "{:?}",
        requests.iter().map(|m| user_texts(m)).collect::<Vec<_>>()
    );
    assert_eq!(added(&llm, 2), vec!["FAST-TWO\n\nFAST-THREE".to_string()]);
    rig.quit().await;
}

/// The run ends before the first message is written (request 0 answers
/// with text): the first runs as the follow-up run, and the held two go
/// at the run's end as one message, at once: no request later than the
/// one after the first's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_messages_go_at_the_run_s_end_as_one() {
    let ws = Workspace::new();
    let first = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&first), Box::new(Step::Text("first answer"))),
        contacts("c1"),
        Step::Text("done"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let agent = rig.agent().await;
    let local = rig.daemon.local();
    rig.send(Effect::StartTurn("go".into()));
    llm.wait_call(0).await;
    typed(&mut rig, 1, "END-ONE").await;
    typed(&mut rig, 2, "END-TWO").await;
    typed(&mut rig, 3, "END-THREE").await;
    assert_eq!(local.held_user_for_test(&agent).len(), 2);
    first.notify_one();
    for id in [1, 2, 3] {
        rig.saw(move |msg| matches!(msg, Msg::SteerDelivered(PendingId(i)) if *i == id))
            .await;
    }
    loop {
        let outcome = rig.turn_finished().await.expect("turn succeeds");
        if outcome.text == "done" {
            break;
        }
    }
    let requests = llm.requests();
    let at = |text: &str| {
        requests
            .iter()
            .position(|messages| format!("{messages:?}").contains(text))
    };
    assert_eq!(at("END-ONE"), Some(1));
    // With the first in the follow-up run's batch (request 1), or, sent
    // after that run began, its next request.
    let held = at("END-TWO");
    assert!(held == Some(1) || held == Some(2), "{held:?}");
    assert_eq!(held, at("END-THREE"));
    let stored = user_texts(&rig.messages().await);
    assert_eq!(
        stored
            .iter()
            .filter(|text| text.contains("END-TWO") || text.contains("END-THREE"))
            .collect::<Vec<_>>(),
        vec!["END-TWO\n\nEND-THREE"],
        "{stored:?}"
    );
    assert_eq!(requests.len(), 3, "{stored:?}");
    rig.quit().await;
}

/// Esc with two messages held behind the one in flight: all three come
/// back to the editor (the held two from the hold, at no cost; the first
/// taken back from the engine), and no request carries any of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupt_returns_the_held_messages_too() {
    let ws = Workspace::new();
    let first = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&first), Box::new(contacts("c0"))),
        Step::Text("after"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let agent = rig.agent().await;
    let local = rig.daemon.local();
    rig.send(Effect::StartTurn("go".into()));
    llm.wait_call(0).await;
    typed(&mut rig, 1, "ESC-ONE").await;
    typed(&mut rig, 2, "ESC-TWO").await;
    typed(&mut rig, 3, "ESC-THREE").await;
    rig.send(Effect::Interrupt);
    let returned = rig.saw(|msg| matches!(msg, Msg::Returned(_))).await;
    let Msg::Returned(mut ids) = returned else {
        unreachable!()
    };
    ids.sort();
    assert_eq!(ids, vec![PendingId(1), PendingId(2), PendingId(3)]);
    assert!(local.held_user_for_test(&agent).is_empty());
    first.notify_one();
    let outcome = rig.turn_finished().await;
    assert!(outcome.is_err(), "{outcome:?}");
    for (at, messages) in llm.requests().iter().enumerate() {
        assert!(
            !format!("{messages:?}").contains("ESC-"),
            "request {at} carries a returned message"
        );
    }
    rig.quit().await;
}

/// The peer id of the agent `name` in the last `contacts` result of
/// `messages`.
fn peer_id_of(messages: &[meerkat_core::Message], name: &str) -> Option<String> {
    fn find(value: &serde_json::Value, name: &str) -> Option<String> {
        match value {
            serde_json::Value::Object(map) => {
                let names_it = map
                    .values()
                    .any(|v| v.as_str().is_some_and(|s| s.contains(name)));
                if names_it && let Some(id) = map.get("peer_id").and_then(|v| v.as_str()) {
                    return Some(id.to_string());
                }
                map.values().find_map(|v| find(v, name))
            }
            serde_json::Value::Array(items) => items.iter().find_map(|v| find(v, name)),
            _ => None,
        }
    }
    messages.iter().rev().find_map(|m| match m {
        meerkat_core::Message::ToolResults { results, .. } => results.iter().find_map(|r| {
            serde_json::from_str::<serde_json::Value>(&r.text_content())
                .ok()
                .and_then(|value| find(&value, name))
        }),
        _ => None,
    })
}

/// Another agent's message to A arrives while A's user has one message
/// in flight and two held: it takes at most the one engine slot between
/// the first and the held two (the held two go as one, so it never gets
/// further ahead of them).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_s_message_gets_at_most_one_slot_ahead_of_the_held_ones() {
    use rkat_tui::daemon::protocol::AgentChoice;
    let ws = Workspace::new();
    let a_gate = Arc::new(Notify::new());
    let a_name = Arc::new(parking_lot::Mutex::new(String::new()));
    let is_a = |messages: &[meerkat_core::Message]| {
        user_texts(messages)
            .iter()
            .any(|text| text.contains("WORK-A"))
    };
    let step = {
        let (a_gate, a_name) = (Arc::clone(&a_gate), Arc::clone(&a_name));
        Step::Dynamic(Arc::new(move |messages: &[meerkat_core::Message]| {
            if is_a(messages) {
                for (answered, next) in [("c2", "c3"), ("c1", "c2"), ("c0", "c1")] {
                    if common::answers(messages, Some(answered)) {
                        return contacts(next);
                    }
                }
                if common::answers(messages, Some("c3")) {
                    return Step::Text("done-a");
                }
                return Step::Gated(Arc::clone(&a_gate), Box::new(contacts("c0")));
            }
            if common::answers(messages, Some("pm")) {
                return Step::Text("told a");
            }
            if common::answers(messages, Some("p1")) {
                let peer = peer_id_of(messages, &a_name.lock()).unwrap_or_else(|| "none".into());
                return Step::Tool {
                    id: "pm",
                    name: "send_message",
                    args: json!({"peer_id": peer, "body": "PEER-NOTE", "handling_mode": "steer"}),
                };
            }
            contacts("p1")
        }))
    };
    let llm = ScriptedLlmClient::new(vec![step; 16]);
    let classifier = rig::ScriptedClassifier::allowing(Vec::new());
    let setup = rig::Setup::default().classifier(Arc::clone(&classifier) as _);
    let args = setup.command_line(&ws.root);
    let daemon = rig::Running::start_with(&args, setup.options(&ws.root, &llm)).await;
    let mut a = rig::Client::attach(&ws.root).await;
    let mut b = rig::Client::attach_agent(&ws.root, AgentChoice::New).await;
    *a_name.lock() = a.agent().await;
    let a_requests = || {
        llm.requests()
            .into_iter()
            .filter(|messages| is_a(messages))
            .collect::<Vec<_>>()
    };
    a.send(Effect::StartTurn("WORK-A".into()));
    common::until(|| a_requests().len() == 1).await;
    for (id, text) in [(1, "PEER-ONE"), (2, "PEER-TWO"), (3, "PEER-THREE")] {
        a.send(steer(id, text));
        a.saw(move |msg| matches!(msg, Msg::SteerAck { id: PendingId(i), result: Ok(()) } if *i == id))
            .await;
    }
    b.send(Effect::StartTurn("TELL-A".into()));
    assert_eq!(b.turn_finished().await.unwrap().text, "told a");
    a_gate.notify_one();
    loop {
        if a.turn_finished()
            .await
            .is_ok_and(|outcome| outcome.text == "done-a")
        {
            break;
        }
    }
    let requests = a_requests();
    let at = |text: &str| {
        requests
            .iter()
            .position(|messages| format!("{messages:?}").contains(text))
    };
    let (first, held, peer) = (
        at("PEER-ONE"),
        at("PEER-TWO\\n\\nPEER-THREE"),
        at("PEER-NOTE"),
    );
    assert_eq!(first, Some(1), "{requests:?}");
    let (held, peer) = (
        held.expect("the held two, together"),
        peer.expect("the peer's message"),
    );
    assert!(
        peer >= held || held - peer == 1,
        "the peer's message rode request {peer}, the held ones {held}"
    );
    // Measured on Meerkat 0.9.18: the peer's message, admitted to the
    // engine while the first waited, joins request 1 with it (every steer
    // waiting at a boundary joins its request); the held two ride the one
    // after it, together. On 0.9.17 the peer's took request 2 and the held
    // two request 3.
    assert_eq!((peer, held), (1, 2));
    a.quit().await;
    b.quit().await;
    daemon.stop().await;
}

/// A script that holds the agent's first model call (no tool result yet)
/// at `gate` the first time only, then calls `contacts` three times and
/// answers "done".
fn held_once(gate: &Arc<Notify>) -> Arc<ScriptedLlmClient> {
    let gate = Arc::clone(gate);
    let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let step = Step::Dynamic(Arc::new(move |messages: &[meerkat_core::Message]| {
        for (answered, next) in [("c1", "c2"), ("c0", "c1")] {
            if common::answers(messages, Some(answered)) {
                return contacts(next);
            }
        }
        if common::answers(messages, Some("c2")) {
            return Step::Text("done");
        }
        if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Step::Gated(Arc::clone(&gate), Box::new(contacts("c0")));
        }
        contacts("c0")
    }));
    ScriptedLlmClient::new(vec![step; 16])
}

/// The client detaches with two messages held: the daemon keeps them and
/// sends them as one when the first is written, as with a client there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_messages_outlive_their_client_s_detach() {
    let ws = Workspace::new();
    let gate = Arc::new(Notify::new());
    let llm = held_once(&gate);
    let daemon = rig::Running::start(&ws.root, &llm).await;
    let mut client = rig::Client::attach(&ws.root).await;
    let agent = client.agent().await;
    client.send(Effect::StartTurn("go".into()));
    llm.wait_call(0).await;
    for (id, text) in [(1, "AWAY-ONE"), (2, "AWAY-TWO"), (3, "AWAY-THREE")] {
        client.send(steer(id, text));
        client
            .saw(move |msg| matches!(msg, Msg::SteerAck { id: PendingId(i), result: Ok(()) } if *i == id))
            .await;
    }
    client.send(Effect::Leave(rkat_tui::ui::Departure::Detach));
    let unsent = client.quit().await;
    assert!(
        unsent.is_none(),
        "held messages are the daemon's, not saved as unsent"
    );
    assert_eq!(daemon.local().held_user_for_test(&agent).len(), 2);
    gate.notify_one();
    common::until(|| llm.requests().len() >= 4).await;
    assert_eq!(added(&llm, 1), vec!["AWAY-ONE".to_string()]);
    assert_eq!(added(&llm, 2), vec!["AWAY-TWO\n\nAWAY-THREE".to_string()]);
    daemon.idle(&agent).await;
    daemon.stop().await;
}

/// The daemon stops with two messages held behind the one in flight. The
/// engine does not deliver even the one in flight after the restart (a
/// steer a stop catches in a run), so the held two are not sent and the
/// one in flight is recalled: their client gets all three back as not
/// sent (to the editor), and they are saved in the realm's unsent file
/// for a user whose client is gone. Nothing reaches the model twice or is
/// lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_messages_come_back_when_the_daemon_stops() {
    let ws = Workspace::new();
    let gate = Arc::new(Notify::new());
    let llm = held_once(&gate);
    let daemon = rig::Running::start(&ws.root, &llm).await;
    let mut client = rig::Client::attach(&ws.root).await;
    let agent = client.agent().await;
    client.send(Effect::StartTurn("go".into()));
    llm.wait_call(0).await;
    for (id, text) in [(1, "STOP-ONE"), (2, "STOP-TWO"), (3, "STOP-THREE")] {
        client.send(steer(id, text));
        client
            .saw(move |msg| matches!(msg, Msg::SteerAck { id: PendingId(i), result: Ok(()) } if *i == id))
            .await;
    }
    assert_eq!(daemon.local().held_user_for_test(&agent).len(), 2);
    daemon.stop().await;
    for id in [2, 3] {
        client
            .saw(move |msg| {
                matches!(msg, Msg::NotDelivered { id: PendingId(i), why: Some(why) }
                    if *i == id && why.contains("the daemon stopped before it sent it"))
            })
            .await;
    }
    client
        .saw(|msg| {
            matches!(msg, Msg::NotDelivered { id: PendingId(1), why: Some(why) }
                if why.contains("the daemon stopped before the agent read it"))
        })
        .await;
    let file = common::realm_of(&ws.root).join(rkat_tui::workbench::unsent::UNSENT_FILE);
    let saved = std::fs::read_to_string(&file).unwrap();
    for text in ["STOP-ONE", "STOP-TWO", "STOP-THREE"] {
        assert_eq!(saved.matches(text).count(), 1, "{saved}");
    }
    assert!(
        !llm.requests()
            .iter()
            .any(|messages| format!("{messages:?}").contains("STOP-TWO")),
        "a held message reached the model"
    );
    client.quit().await;
}

/// How many times `text` shows in the stored conversation of `agent`.
async fn stored_count(daemon: &rig::Running, agent: &str, text: &str) -> usize {
    let session = daemon.session_of(agent).await;
    let stored = daemon.transcript(&session).await;
    user_texts(&stored)
        .iter()
        .map(|said| said.matches(text).count())
        .sum()
}

/// Whether `msg` says the client's steer `id` was not delivered.
fn not_delivered(msg: &Msg, id: u64) -> bool {
    matches!(msg, Msg::NotDelivered { id: PendingId(i), .. } if *i == id)
}

/// The daemon stops while the one message in flight waits for the next
/// request (request 0 is held, so no request carries it). The engine
/// would keep it queued and never deliver it after the restart
/// (`docs/evidence.md`, "A message in flight at a daemon stop"): the stop
/// recalls it, its client gets it back as not sent, exactly once, and it
/// is saved in the realm's unsent file; after the restart no request and
/// no stored message holds it, and the engine records it as taken back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_in_flight_at_a_stop_comes_back_once_and_is_never_delivered() {
    let ws = Workspace::new();
    let gate = Arc::new(Notify::new());
    let llm = held_once(&gate);
    let daemon = rig::Running::start(&ws.root, &llm).await;
    let mut client = rig::Client::attach(&ws.root).await;
    let agent = client.agent().await;
    client.send(Effect::StartTurn("go".into()));
    llm.wait_call(0).await;
    client.send(steer(1, "FLIGHT-ONE"));
    client
        .saw(|msg| {
            matches!(
                msg,
                Msg::SteerAck {
                    id: PendingId(1),
                    result: Ok(())
                }
            )
        })
        .await;
    let local = daemon.local();
    common::until(|| local.in_flight_for_test(&agent).is_some()).await;
    let input = local.in_flight_for_test(&agent).unwrap();
    drop(local);
    daemon.stop().await;
    let back = client.saw(|msg| not_delivered(msg, 1)).await;
    let Msg::NotDelivered { why: Some(why), .. } = back else {
        panic!("{back:?}")
    };
    assert!(
        why.contains("the daemon stopped before the agent read it"),
        "{why}"
    );
    let file = common::realm_of(&ws.root).join(rkat_tui::workbench::unsent::UNSENT_FILE);
    let saved = std::fs::read_to_string(&file).unwrap();
    assert_eq!(saved.matches("FLIGHT-ONE").count(), 1, "{saved}");
    // The restart resumes the stopped run; a later prompt runs too.
    let daemon = rig::Running::start(&ws.root, &llm).await;
    client.attached().await;
    daemon.idle(&agent).await;
    client.send(Effect::StartTurn("LATER".into()));
    common::until(|| {
        llm.requests()
            .iter()
            .any(|m| user_texts(m).iter().any(|t| t == "LATER"))
    })
    .await;
    daemon.idle(&agent).await;
    let fates = daemon
        .connect_bound()
        .await
        .call("rkat/inputs", json!({"input_ids": [input]}))
        .await
        .unwrap();
    assert!(
        !llm.requests()
            .iter()
            .any(|messages| format!("{messages:?}").contains("FLIGHT-ONE")),
        "the message came back and reached the model too"
    );
    assert_eq!(stored_count(&daemon, &agent, "FLIGHT-ONE").await, 0);
    assert_eq!(
        client
            .seen
            .iter()
            .filter(|msg| not_delivered(msg, 1))
            .count(),
        1,
        "offered back once"
    );
    assert!(
        format!("{fates}").contains("dropped"),
        "the engine's record of it: {fates}"
    );
    client.quit().await;
    daemon.stop().await;
}

/// What a stop after the message in flight was written into request 1
/// left: whether its client got it back (and the notice's reason), the
/// unsent file, how often the stored conversation holds it after the
/// restart, whether a request after the restart carried it, and the
/// engine's record of it then. `committed`: request 1 is answered with a
/// tool, which runs (the round is committed), and the stop comes during
/// request 2; else during request 1.
struct WrittenAtStop {
    unkept: Vec<String>,
    saved: String,
    stored: usize,
    carried_after: bool,
    fate: String,
}

async fn stop_after_written(committed: bool, text: &'static str) -> WrittenAtStop {
    let ws = Workspace::new();
    let (first, second) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&first), Box::new(contacts("c0"))),
        Step::Gated(Arc::clone(&second), Box::new(contacts("c1"))),
        Step::Gated(Arc::new(Notify::new()), Box::new(contacts("c2"))),
        Step::Text("done"),
        Step::Text("done"),
        Step::Text("done"),
    ]);
    let daemon = rig::Running::start(&ws.root, &llm).await;
    let mut client = rig::Client::attach(&ws.root).await;
    let agent = client.agent().await;
    client.send(Effect::StartTurn("go".into()));
    llm.wait_call(0).await;
    client.send(steer(1, text));
    client
        .saw(|msg| {
            matches!(
                msg,
                Msg::SteerAck {
                    id: PendingId(1),
                    result: Ok(())
                }
            )
        })
        .await;
    let local = daemon.local();
    common::until(|| local.in_flight_for_test(&agent).is_some()).await;
    let input = local.in_flight_for_test(&agent).unwrap();
    first.notify_one();
    client
        .saw(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))))
        .await;
    llm.wait_call(1).await;
    assert!(carried(&llm, 1, text));
    if committed {
        second.notify_one();
        llm.wait_call(2).await;
        // The daemon read the tool's start (the round's commit).
        common::until(|| local.written_for_test(&agent) == 0).await;
    } else {
        assert_eq!(local.written_for_test(&agent), 1);
    }
    drop(local);
    daemon.stop().await;
    client
        .saw(|msg| {
            matches!(
                msg,
                Msg::Status(rkat_tui::ui::StatusUpdate::Connection(
                    rkat_tui::ui::Connection::Reconnecting
                ))
            )
        })
        .await;
    let unkept = client
        .seen
        .iter()
        .filter_map(|msg| match msg {
            Msg::Unkept { text, why } => Some(format!("{text}: {why}")),
            _ => None,
        })
        .collect();
    assert!(
        !client.seen.iter().any(|msg| not_delivered(msg, 1)),
        "a delivered message was told as not delivered"
    );
    let file = common::realm_of(&ws.root).join(rkat_tui::workbench::unsent::UNSENT_FILE);
    let saved = std::fs::read_to_string(&file).unwrap_or_default();
    let before = llm.requests().len();
    let daemon = rig::Running::start(&ws.root, &llm).await;
    client.attached().await;
    daemon.idle(&agent).await;
    let stored = stored_count(&daemon, &agent, text).await;
    let carried_after = llm.requests()[before..]
        .iter()
        .any(|messages| user_texts(messages).iter().any(|said| said.contains(text)));
    let fate = daemon
        .connect_bound()
        .await
        .call("rkat/inputs", json!({"input_ids": [input]}))
        .await
        .unwrap()
        .to_string();
    client.quit().await;
    daemon.stop().await;
    WrittenAtStop {
        unkept,
        saved,
        stored,
        carried_after,
        fate,
    }
}

/// The message in flight is written into request 1, and the daemon stops
/// while request 1 is held: the engine commits a round only after its
/// request's answer, and drops the uncommitted steer at the restart
/// (recorded "the agent's runtime was stopped"; the stored conversation
/// does not hold it). So its client, which saw it delivered, gets it back
/// once, and it is saved as unsent; nothing after the restart carries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_written_into_a_request_the_stop_cut_off_comes_back_once() {
    let seen = stop_after_written(false, "CUT-ONE").await;
    assert_eq!(seen.unkept.len(), 1, "{:?}", seen.unkept);
    assert!(
        seen.unkept[0].starts_with("CUT-ONE: the daemon stopped before the agent read it"),
        "{:?}",
        seen.unkept
    );
    assert_eq!(seen.saved.matches("CUT-ONE").count(), 1, "{}", seen.saved);
    assert_eq!(seen.stored, 0, "the engine kept it after all");
    assert!(!seen.carried_after);
    assert!(
        seen.fate.contains("dropped") && seen.fate.contains("runtime was stopped"),
        "{}",
        seen.fate
    );
}

/// The opposite order: request 1, which carried the message, is answered
/// with a tool that runs (its round committed), and the daemon stops
/// during request 2. The message stays the agent's: not offered back, not
/// in the unsent file, and in the stored conversation once after the
/// restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_whose_round_was_committed_stays_in_the_conversation_once() {
    let seen = stop_after_written(true, "KEPT-ONE").await;
    assert!(seen.unkept.is_empty(), "{:?}", seen.unkept);
    assert!(!seen.saved.contains("KEPT-ONE"), "{}", seen.saved);
    assert_eq!(seen.stored, 1);
    assert!(seen.fate.contains("taken"), "{}", seen.fate);
}

/// ↑ takes back a held message for free: it leaves the hold, the one in
/// flight is untouched; edited and sent again it is held again at the
/// end, and goes with the other held one, in that order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_message_taken_back_and_sent_again_is_held_at_the_end() {
    let ws = Workspace::new();
    let first = Arc::new(Notify::new());
    let second = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&first), Box::new(contacts("c0"))),
        Step::Gated(Arc::clone(&second), Box::new(contacts("c1"))),
        contacts("c2"),
        Step::Text("done"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let agent = rig.agent().await;
    let local = rig.daemon.local();
    rig.send(Effect::StartTurn("go".into()));
    llm.wait_call(0).await;
    typed(&mut rig, 1, "BACK-ONE").await;
    typed(&mut rig, 2, "BACK-TWO").await;
    typed(&mut rig, 3, "BACK-THREE").await;
    rig.send(Effect::RecallHeld(PendingId(3)));
    rig.saw(|msg| {
        matches!(msg, Msg::HeldTakenBack { id: PendingId(3), taken: true, errors } if errors.is_empty())
    })
    .await;
    assert_eq!(
        local.held_user_for_test(&agent),
        vec!["BACK-TWO".to_string()]
    );
    typed(&mut rig, 4, "EDITED-THREE").await;
    rig.saw(|msg| matches!(msg, Msg::SteerHeld(PendingId(4))))
        .await;
    assert_eq!(
        local.held_user_for_test(&agent),
        vec!["BACK-TWO".to_string(), "EDITED-THREE".to_string()]
    );
    first.notify_one();
    llm.wait_call(1).await;
    assert_eq!(added(&llm, 1), vec!["BACK-ONE".to_string()]);
    rig.saw(|msg| matches!(msg, Msg::SteerReleased(ids) if *ids == [PendingId(2), PendingId(4)]))
        .await;
    second.notify_one();
    llm.wait_call(2).await;
    assert_eq!(added(&llm, 2), vec!["BACK-TWO\n\nEDITED-THREE".to_string()]);
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "done");
    assert!(
        !llm.requests()
            .iter()
            .any(|messages| format!("{messages:?}").contains("BACK-THREE")),
        "the message taken back reached the model"
    );
    rig.quit().await;
}

/// With nothing held, ↑ takes back the one in flight from the engine, as
/// before; a held one taken back first leaves it alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_one_in_flight_is_taken_back_only_once_nothing_is_held() {
    let ws = Workspace::new();
    let first = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&first), Box::new(contacts("c0"))),
        contacts("c1"),
        Step::Text("done"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("go".into()));
    llm.wait_call(0).await;
    typed(&mut rig, 1, "LAST-ONE").await;
    typed(&mut rig, 2, "LAST-TWO").await;
    rig.send(Effect::RecallHeld(PendingId(2)));
    rig.saw(|msg| {
        matches!(
            msg,
            Msg::HeldTakenBack {
                id: PendingId(2),
                taken: true,
                ..
            }
        )
    })
    .await;
    assert!(
        !rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::Recalled { .. })),
        "the one in flight is untouched"
    );
    rig.send(Effect::RecallQueued);
    rig.saw(|msg| matches!(msg, Msg::Recalled { recalled, .. } if *recalled == [PendingId(1)]))
        .await;
    first.notify_one();
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "done");
    assert!(
        !llm.requests()
            .iter()
            .any(|messages| format!("{messages:?}").contains("LAST-")),
        "a message taken back reached the model"
    );
    rig.quit().await;
}

/// How request 0 of a hold-first round answers once released.
#[derive(Debug, Clone, Copy)]
enum FirstCall {
    /// One instant tool.
    OneTool,
    /// Three instant tools in parallel.
    ParallelTools,
    /// Text: the run ends.
    Text,
}

/// One round of the hold-first measurement: the daemon holds even the
/// first message typed while request 0 streams, and sends it at that
/// call's first tool request (or the run's end); the request that first
/// carries it.
async fn hold_first_round(call: FirstCall) -> Option<usize> {
    let ws = Workspace::new();
    let gate = Arc::new(Notify::new());
    let answer = match call {
        FirstCall::OneTool => contacts("c0"),
        FirstCall::ParallelTools => Step::Tools(vec![
            ("p0", "contacts", json!({})),
            ("p1", "contacts", json!({})),
            ("p2", "contacts", json!({})),
        ]),
        FirstCall::Text => Step::Text("first answer"),
    };
    let llm = ScriptedLlmClient::new(vec![
        Step::Streaming("part", Arc::clone(&gate), Box::new(answer)),
        contacts("c1"),
        Step::Text("done"),
        Step::Text("done"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.daemon.local().hold_first_for_test(true);
    rig.send(Effect::StartTurn("go".into()));
    rig.saw(|msg| {
        matches!(msg, Msg::Agent(env) if matches!(&env.payload, meerkat_core::AgentEvent::TextDelta { delta, .. } if delta == "part"))
    })
    .await;
    typed(&mut rig, 1, "FIRST-HELD").await;
    rig.saw(|msg| matches!(msg, Msg::SteerHeld(PendingId(1))))
        .await;
    gate.notify_one();
    rig.saw(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))))
        .await;
    let at = llm
        .requests()
        .iter()
        .position(|messages| format!("{messages:?}").contains("FIRST-HELD"));
    rig.quit().await;
    at
}

/// The measurement of holding the first message too (not shipped:
/// `docs/evidence.md`, "Holding the first message too"): 20 rounds of
/// each kind of first call, the request that carried the message.
/// `--run-ignored only -E 'test(hold_first)' --no-capture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement, not a test of shipped behaviour"]
async fn hold_first_measurement() {
    for call in [
        FirstCall::OneTool,
        FirstCall::ParallelTools,
        FirstCall::Text,
    ] {
        let mut rides = std::collections::BTreeMap::<String, usize>::new();
        for _ in 0..20 {
            let at = hold_first_round(call).await;
            *rides.entry(format!("{at:?}")).or_default() += 1;
        }
        eprintln!("HOLD-FIRST {call:?}: {rides:?}");
    }
}

/// Three messages typed while request 0 is in flight: the first rides
/// request 1; the second and third are held in the daemon (each told so)
/// and ride request 2 together, in order, as one user message, sent once
/// the first was written (each told so), and each is marked delivered.
/// Request 1 is held until the client hears that they were sent: the
/// order is the rig's, so the test pins what goes where, not a race.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_quick_messages_ride_the_next_two_requests() {
    let ws = Workspace::new();
    let first = Arc::new(Notify::new());
    let second = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&first), Box::new(contacts("c0"))),
        Step::Gated(Arc::clone(&second), Box::new(contacts("c1"))),
        contacts("c2"),
        Step::Text("done"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let agent = rig.agent().await;
    let local = rig.daemon.local();
    rig.send(Effect::StartTurn("go".into()));
    llm.wait_call(0).await;
    typed(&mut rig, 1, "QUICK-ONE").await;
    typed(&mut rig, 2, "QUICK-TWO").await;
    rig.saw(|msg| matches!(msg, Msg::SteerHeld(PendingId(2))))
        .await;
    typed(&mut rig, 3, "QUICK-THREE").await;
    rig.saw(|msg| matches!(msg, Msg::SteerHeld(PendingId(3))))
        .await;
    assert!(
        !rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::SteerHeld(PendingId(1)))),
        "the first is in flight, not held"
    );
    assert_eq!(
        local.held_user_for_test(&agent),
        vec!["QUICK-TWO".to_string(), "QUICK-THREE".to_string()]
    );
    first.notify_one();
    llm.wait_call(1).await;
    assert!(carried(&llm, 1, "QUICK-ONE"), "{:?}", llm.requests()[1]);
    assert!(!carried(&llm, 1, "QUICK-TWO") && !carried(&llm, 1, "QUICK-THREE"));
    rig.saw(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))))
        .await;
    rig.saw(|msg| matches!(msg, Msg::SteerReleased(ids) if *ids == [PendingId(2), PendingId(3)]))
        .await;
    assert!(local.held_user_for_test(&agent).is_empty());
    second.notify_one();
    llm.wait_call(2).await;
    assert_eq!(
        added(&llm, 2),
        vec!["QUICK-TWO\n\nQUICK-THREE".to_string()],
        "the two held ones in one user message, in order"
    );
    for id in [2, 3] {
        rig.saw(move |msg| matches!(msg, Msg::SteerDelivered(PendingId(i)) if *i == id))
            .await;
    }
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "done");
    assert_eq!(llm.requests().len(), 4, "one run, no follow-up");
    // The stored conversation has each text once: the held two as one
    // user message.
    let stored = user_texts(&rig.messages().await);
    assert_eq!(
        stored,
        vec![
            "go".to_string(),
            "QUICK-ONE".to_string(),
            "QUICK-TWO\n\nQUICK-THREE".to_string()
        ]
    );
    rig.quit().await;
}
