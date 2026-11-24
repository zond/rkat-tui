//! The user's messages typed while the agent works: each goes to the
//! engine at once as a plain steer, and every one waiting when a model
//! request's input is fixed joins that request, in the order typed
//! (Meerkat 0.9.18; `docs/evidence.md`, "Every waiting steer joins the
//! next request"). A daemon stop gives back the ones the engine would
//! lose (`rkat_tui::daemon::activity::typed`). Through the real daemon
//! with a scripted model whose requests are held, and an attached client.

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

/// Whether any request holds `text`.
fn reached(llm: &ScriptedLlmClient, text: &str) -> bool {
    llm.requests()
        .iter()
        .any(|messages| format!("{messages:?}").contains(text))
}

/// Type `text` as message `id` and wait for its acknowledgement.
async fn typed(rig: &mut Attached, id: u64, text: &str) {
    rig.send(steer(id, text));
    rig.saw(
        move |msg| matches!(msg, Msg::SteerAck { id: PendingId(i), result: Ok(()) } if *i == id),
    )
    .await;
}

/// Whether `msg` marks the client's steer `id` delivered.
fn delivered(msg: &Msg, id: u64) -> bool {
    matches!(msg, Msg::SteerDelivered(PendingId(i)) if *i == id)
}

/// Three messages typed while request 0 is in flight all ride request 1,
/// each its own user message, in the order typed; none is shown
/// delivered before request 1 starts, each is once it has; the stored
/// conversation holds each once; the run needs no follow-up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_quick_messages_ride_the_next_request_in_order() {
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
    typed(&mut rig, 1, "QUICK-ONE").await;
    typed(&mut rig, 2, "QUICK-TWO").await;
    typed(&mut rig, 3, "QUICK-THREE").await;
    assert!(
        !rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::SteerDelivered(_))),
        "shown delivered before a request carried it"
    );
    first.notify_one();
    for id in [1, 2, 3] {
        rig.saw(move |msg| delivered(msg, id)).await;
    }
    assert!(llm.requests().len() >= 2);
    assert_eq!(
        added(&llm, 1),
        vec![
            "QUICK-ONE".to_string(),
            "QUICK-TWO".to_string(),
            "QUICK-THREE".to_string()
        ]
    );
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "done");
    assert_eq!(llm.requests().len(), 3, "one run, no follow-up");
    assert_eq!(
        user_texts(&rig.messages().await),
        vec!["go", "QUICK-ONE", "QUICK-TWO", "QUICK-THREE"]
    );
    rig.quit().await;
}

/// The run ends before the messages went in (request 0 answers with
/// text): all three start the follow-up run together, once each.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_typed_as_the_run_ends_start_the_next_run_together() {
    let ws = Workspace::new();
    let first = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&first), Box::new(Step::Text("first answer"))),
        Step::Text("done"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("go".into()));
    llm.wait_call(0).await;
    typed(&mut rig, 1, "END-ONE").await;
    typed(&mut rig, 2, "END-TWO").await;
    typed(&mut rig, 3, "END-THREE").await;
    first.notify_one();
    for id in [1, 2, 3] {
        rig.saw(move |msg| delivered(msg, id)).await;
    }
    loop {
        let outcome = rig.turn_finished().await.expect("turn succeeds");
        if outcome.text == "done" {
            break;
        }
    }
    for text in ["END-ONE", "END-TWO", "END-THREE"] {
        assert!(carried(&llm, 1, text), "{text}: {:?}", llm.requests()[1]);
    }
    assert_eq!(llm.requests().len(), 2);
    let stored = user_texts(&rig.messages().await);
    for text in ["END-ONE", "END-TWO", "END-THREE"] {
        assert_eq!(
            stored.iter().filter(|said| said.contains(text)).count(),
            1,
            "{stored:?}"
        );
    }
    rig.quit().await;
}

/// Esc with three messages waiting: all three come back to the editor,
/// and no request carries any of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupt_returns_every_waiting_message() {
    let ws = Workspace::new();
    let first = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&first), Box::new(contacts("c0"))),
        Step::Text("after"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
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
    first.notify_one();
    let outcome = rig.turn_finished().await;
    assert!(outcome.is_err(), "{outcome:?}");
    assert!(
        !reached(&llm, "ESC-"),
        "a returned message reached the model"
    );
    rig.quit().await;
}

/// ↑ takes back every message still waiting (the engine had not written
/// them): none reaches the model, and what is typed instead rides the
/// very next request, which the recall did not leave empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_taken_back_cost_the_next_one_nothing() {
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
    typed(&mut rig, 1, "BACK-ONE").await;
    typed(&mut rig, 2, "BACK-TWO").await;
    rig.send(Effect::RecallQueued);
    let recalled = rig.saw(|msg| matches!(msg, Msg::Recalled { .. })).await;
    let Msg::Recalled { recalled, errors } = recalled else {
        unreachable!()
    };
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(recalled, vec![PendingId(1), PendingId(2)]);
    typed(&mut rig, 3, "EDITED").await;
    first.notify_one();
    rig.saw(|msg| delivered(msg, 3)).await;
    assert_eq!(added(&llm, 1), vec!["EDITED".to_string()]);
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "done");
    assert!(
        !reached(&llm, "BACK-"),
        "a message taken back reached the model"
    );
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

/// Another agent's message to A arrives while three of A's user's
/// messages wait: it joins the same request, after them (admitted after
/// them); it never pushes them to a later request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_s_message_joins_the_request_the_user_s_wait_for() {
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
                if common::answers(messages, Some("c1")) {
                    return Step::Text("done-a");
                }
                if common::answers(messages, Some("c0")) {
                    return contacts("c1");
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
    assert_eq!(
        [
            at("PEER-ONE"),
            at("PEER-TWO"),
            at("PEER-THREE"),
            at("PEER-NOTE")
        ],
        [Some(1); 4],
        "{requests:?}"
    );
    let joined = format!("{:?}", requests[1]);
    let order: Vec<usize> = ["PEER-ONE", "PEER-TWO", "PEER-THREE", "PEER-NOTE"]
        .iter()
        .map(|text| joined.find(text).unwrap())
        .collect();
    assert!(order.is_sorted(), "admission order: {order:?}");
    a.quit().await;
    b.quit().await;
    daemon.stop().await;
}

/// A note of the daemon's own and a message the user types while the
/// same request is in flight ride the next request together, in the
/// order they reached the engine, whichever comes first: a note never
/// costs the user's message a request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_of_ours_never_costs_the_user_s_message_a_request() {
    for note_first in [true, false] {
        let ws = Workspace::new();
        let first = Arc::new(Notify::new());
        let llm = ScriptedLlmClient::new(vec![
            Step::Gated(Arc::clone(&first), Box::new(contacts("c0"))),
            contacts("c1"),
            Step::Text("done"),
        ]);
        let mut rig = Attached::start(&ws.root, &llm).await;
        let agent = rig.agent().await;
        let local = rig.daemon.local();
        rig.send(Effect::StartTurn("go".into()));
        llm.wait_call(0).await;
        if note_first {
            local.steer_for_test(&agent, "OUR-NOTE").await.unwrap();
            typed(&mut rig, 1, "USER-WORDS").await;
        } else {
            typed(&mut rig, 1, "USER-WORDS").await;
            local.steer_for_test(&agent, "OUR-NOTE").await.unwrap();
        }
        first.notify_one();
        rig.saw(|msg| delivered(msg, 1)).await;
        let added = added(&llm, 1);
        let expected = if note_first {
            ["OUR-NOTE", "USER-WORDS"]
        } else {
            ["USER-WORDS", "OUR-NOTE"]
        };
        assert_eq!(added, expected, "note first: {note_first}");
        let outcome = rig.turn_finished().await;
        assert_eq!(outcome.expect("turn succeeds").text, "done");
        drop(local);
        rig.quit().await;
    }
}

/// A script that holds the agent's first model call (no tool result yet)
/// at `gate` the first time only, then calls `contacts` twice and
/// answers "done".
fn held_once(gate: &Arc<Notify>) -> Arc<ScriptedLlmClient> {
    let gate = Arc::clone(gate);
    let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let step = Step::Dynamic(Arc::new(move |messages: &[meerkat_core::Message]| {
        if common::answers(messages, Some("c1")) {
            return Step::Text("done");
        }
        if common::answers(messages, Some("c0")) {
            return contacts("c1");
        }
        if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Step::Gated(Arc::clone(&gate), Box::new(contacts("c0")));
        }
        contacts("c0")
    }));
    ScriptedLlmClient::new(vec![step; 16])
}

/// The client detaches with three messages waiting: they are the
/// engine's, not saved as unsent, and ride the next request as usual.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_messages_outlive_their_client_s_detach() {
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
        "waiting messages are the agent's, not saved as unsent"
    );
    gate.notify_one();
    common::until(|| llm.requests().len() >= 2).await;
    assert_eq!(
        added(&llm, 1),
        vec![
            "AWAY-ONE".to_string(),
            "AWAY-TWO".to_string(),
            "AWAY-THREE".to_string()
        ]
    );
    daemon.idle(&agent).await;
    daemon.stop().await;
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

/// The daemon stops while three messages wait for the next request
/// (request 0 is held, so no request carries them). The engine would keep
/// them queued and never deliver them after the restart
/// (`docs/evidence.md`, "A message in flight at a daemon stop"): the stop
/// recalls each, its client gets each back as not read, exactly once,
/// and each is saved in the realm's unsent file once; after the restart
/// no request and no stored message holds any of them, and the engine
/// records them as taken back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_messages_at_a_stop_come_back_once_and_are_never_delivered() {
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
    let local = daemon.local();
    common::until(|| local.waiting_for_test(&agent).len() == 3).await;
    let inputs = local.waiting_for_test(&agent);
    drop(local);
    daemon.stop().await;
    for id in [1, 2, 3] {
        client
            .saw(move |msg| {
                matches!(msg, Msg::NotDelivered { id: PendingId(i), why: Some(why) }
                    if *i == id && why.contains("the daemon stopped before the agent read it"))
            })
            .await;
    }
    let file = common::realm_of(&ws.root).join(rkat_tui::workbench::unsent::UNSENT_FILE);
    let saved = std::fs::read_to_string(&file).unwrap();
    for text in ["STOP-ONE", "STOP-TWO", "STOP-THREE"] {
        assert_eq!(saved.matches(text).count(), 1, "{saved}");
    }
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
        .call("rkat/inputs", json!({"input_ids": inputs}))
        .await
        .unwrap();
    assert!(
        !reached(&llm, "STOP-"),
        "a message came back and reached the model too"
    );
    assert_eq!(stored_count(&daemon, &agent, "STOP-").await, 0);
    for id in [1, 2, 3] {
        assert_eq!(
            client
                .seen
                .iter()
                .filter(|msg| not_delivered(msg, id))
                .count(),
            1,
            "offered back once"
        );
    }
    assert_eq!(
        format!("{fates}").matches("dropped").count(),
        3,
        "the engine's record of them: {fates}"
    );
    client.quit().await;
    daemon.stop().await;
}

/// What a stop after the message was written into request 1 left: whether
/// its client got it back (and the notice's reason), the unsent file, how
/// often the stored conversation holds it after the restart, whether a
/// request after the restart carried it, and the engine's record of it
/// then. `committed`: request 1 is answered with a tool, which runs (the
/// round is committed), and the stop comes during request 2; else during
/// request 1.
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
    common::until(|| local.waiting_for_test(&agent).len() == 1).await;
    let input = local.waiting_for_test(&agent).remove(0);
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

/// The message is written into request 1, and the daemon stops while
/// request 1 is held: the engine commits a round only after its request's
/// answer, and drops the uncommitted steer at the restart (recorded "the
/// agent's runtime was stopped"; the stored conversation does not hold
/// it). So its client, which saw it delivered, gets it back once, and it
/// is saved as unsent; nothing after the restart carries it.
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
