//! The client's turns end to end, headless: effects in, UI messages out, through the
//! `rkat-tui-mob` daemon with a scripted LLM and an attached client. Covers
//! a turn, a queued turn, a steered message applied at a tool boundary
//! (and one typed while a slow shell call runs), interrupt (also during a
//! slow tool), `-c` and `--resume` across daemon lifetimes, and a moved
//! workspace offered its old state (the re-home is `tests/rehome.rs`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, copy_tree, task_id, user_texts};
use meerkat_core::{AgentEvent, Message};
use rig::{Attached, ScriptedClassifier, Setup};
use rkat_tui::approval::classifier::{ActionClassifier, ClassifierVerdict};
use rkat_tui::bootstrap::realm_home;
use rkat_tui::ui::delivery::PendingId;
use rkat_tui::ui::{Block, Effect, EscalationAnswer, Msg, StatusUpdate, TurnFailure};
use serde_json::json;
use tokio::sync::Notify;

/// A daemon setup whose client starts with `args` (`-c`, `--resume`).
fn client_args(args: &[&str]) -> Setup {
    Setup {
        client_args: args.iter().map(ToString::to_string).collect(),
        ..Setup::default()
    }
}

fn steer(id: u64, text: &str) -> Effect {
    Effect::Steer {
        id: PendingId(id),
        text: text.into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creates_a_session_and_completes_a_turn() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("hello there")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    // The workspace's realm lives outside it, under the config directory
    // (the rig's user config root is the workspace, so the engine's
    // `.rkat/session_comms_identity` lands there; no realm does).
    let realm = common::realm_of(&ws.root);
    assert!(realm.starts_with(common::config_dir_for(&ws.root)));
    assert!(
        realm
            .join(rkat_tui::bootstrap::realm_home::MANIFEST_FILE)
            .is_file()
    );
    assert!(!ws.root.join(".rkat").join("tui").exists());

    rig.send(Effect::StartTurn("hi".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "hello there");

    let session = rig.messages().await;
    assert_eq!(user_texts(&session), vec!["hi".to_string()]);
    // The persona reached the model.
    let first = &llm.requests()[0];
    assert!(first.iter().any(|m| matches!(
        m,
        Message::System(system) if system.content.contains("interactive coding agent")
    )));
    assert!(rig.quit().await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_at_mention_attaches_the_file_the_session_receives() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("notes.txt"), "the build uses make\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("read it")]);
    std::fs::create_dir_all(ws.root.join(".rkat")).unwrap();
    std::fs::write(ws.root.join(".rkat/config.toml"), "").unwrap();
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn(
        "what does @notes.txt say? (not @.rkat/config.toml or @nothing.txt)".into(),
    ));
    let _ = rig.turn_finished().await;
    // The UI hears what was attached as it was sent, not what it guessed.
    assert!(rig.seen.iter().any(|msg| matches!(
        msg,
        Msg::Mentions { attached, sealed }
            if attached == &["notes.txt".to_string()]
                && sealed == &[".rkat/config.toml".to_string()]
    )));
    let expected = vec![
        "what does @notes.txt say? (not @.rkat/config.toml or @nothing.txt)\n\n<file path=\"notes.txt\">\nthe build uses make\n</file>"
            .to_string(),
    ];
    assert_eq!(user_texts(&llm.requests()[0]), expected);
    let session = rig.messages().await;
    assert_eq!(user_texts(&session), expected);
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_queued_behind_a_running_turn_runs_next() {
    let ws = Workspace::new();
    let gate = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&gate), Box::new(Step::Text("first answer"))),
        Step::Text("second answer"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("one".into()));
    llm.wait_call(0).await;
    // A second prompt while the first runs: a `Queue` input behind it.
    rig.send(Effect::StartTurn("two".into()));
    gate.notify_one();

    // The second prompt's send races the gate's release: it joins the
    // running turn or runs right after it; either way it is answered next.
    let mut outcome = rig.turn_finished().await;
    if outcome.as_ref().is_ok_and(|s| s.text == "first answer") {
        outcome = rig.turn_finished().await;
    }
    assert_eq!(outcome.expect("turn succeeds").text, "second answer");
    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        user_texts(&requests[1]),
        vec!["one".to_string(), "two".to_string()]
    );
    let session = rig.messages().await;
    assert_eq!(
        user_texts(&session),
        vec!["one".to_string(), "two".to_string()]
    );
    rig.quit().await;
}

/// The owner's rule: what the user types during a run goes
/// in at the next spot, the first model request that starts after it was
/// sent. Measured through the client: a message typed (a) while the tools
/// of step 0 run (a reviewed call whose review is held) rides request 1;
/// (b) typed while request 1 is in flight before any text rides request 2,
/// and is marked delivered while request 2 is still streaming (the mark
/// comes when the carrying request starts, not when its answer ends); (c)
/// typed while request 2 streams text rides request 3. (d) The engine
/// stages one steer per model request (lukacf/meerkat#2431), oldest first: two typed
/// while request 3 is in flight ride requests 4 and 5. Pinned: the day a
/// release lets every waiting steer join the next request, (d) fails and
/// the pending line's words change (`docs/evidence.md`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_typed_message_rides_the_first_request_after_it() {
    let ws = Workspace::new();
    let review = Arc::new(Notify::new());
    let in_flight = Arc::new(Notify::new());
    let streaming = Arc::new(Notify::new());
    let unknown = meerkat_comms::Keypair::generate()
        .public_key()
        .to_peer_id()
        .to_string();
    let contacts = |id: &'static str| Step::Tool {
        id,
        name: "contacts",
        args: json!({}),
    };
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "s0",
            name: "send_message",
            args: json!({"peer_id": unknown, "body": "x", "handling_mode": "queue"}),
        },
        Step::Gated(Arc::clone(&in_flight), Box::new(contacts("c1"))),
        Step::Streaming("part", Arc::clone(&streaming), Box::new(contacts("c2"))),
        Step::Gated(Arc::clone(&in_flight), Box::new(contacts("c3"))),
        contacts("c4"),
        Step::Gated(Arc::clone(&in_flight), Box::new(contacts("c5"))),
        // Its review is held.
        Step::Gated(
            Arc::clone(&in_flight),
            Box::new(Step::Tool {
                id: "s6",
                name: "send_message",
                args: json!({"peer_id": unknown, "body": "y", "handling_mode": "queue"}),
            }),
        ),
        // Its context crosses the prepare threshold.
        Step::ToolUsage {
            id: "c7",
            name: "contacts",
            args: json!({}),
            input: 30_000,
        },
        Step::Gated(Arc::clone(&in_flight), Box::new(contacts("c8"))),
        Step::Text("done"),
    ]);
    let classifier = ScriptedClassifier::holding(Arc::clone(&review));
    let mut rig = Attached::start_with(
        &ws.root,
        &llm,
        Setup::args(&[
            "--set",
            "auto.compact=auto",
            "--set",
            "auto.compact_auto_percent=5",
            "--set",
            "auto.compact_warn_percent=5",
            "--set",
            &format!("auto.compact_prepare={PREPARE}"),
            "--set",
            "auto.compact_prepare_at=20000",
        ])
        .classifier(Arc::clone(&classifier) as _),
    )
    .await;
    let carried = |llm: &ScriptedLlmClient, at: usize, text: &str| {
        llm.requests()
            .get(at)
            .is_some_and(|messages| format!("{messages:?}").contains(text))
    };
    rig.send(Effect::StartTurn("go".into()));
    // (a) The review of step 0's call is held: its tools run.
    common::until(|| !classifier.seen().is_empty()).await;
    rig.send(steer(1, "TYPED-A"));
    rig.wait_for(|msg| {
        matches!(
            msg,
            Msg::SteerAck {
                id: PendingId(1),
                result: Ok(())
            }
        )
    })
    .await;
    review.notify_one();
    llm.wait_call(1).await;
    assert!(carried(&llm, 1, "TYPED-A"), "{:?}", llm.requests()[1]);
    // (b) Request 1 is in flight, no text yet.
    rig.send(steer(2, "TYPED-B"));
    rig.wait_for(|msg| {
        matches!(
            msg,
            Msg::SteerAck {
                id: PendingId(2),
                result: Ok(())
            }
        )
    })
    .await;
    in_flight.notify_one();
    llm.wait_call(2).await;
    assert!(!carried(&llm, 1, "TYPED-B"));
    assert!(carried(&llm, 2, "TYPED-B"), "{:?}", llm.requests()[2]);
    // Marked while request 2 is still held mid-stream.
    rig.wait_for(|msg| matches!(msg, Msg::SteerDelivered(PendingId(2))))
        .await;
    // (c) Request 2 streams text.
    rig.wait_for(|msg| {
        matches!(msg, Msg::Agent(env) if matches!(&env.payload, AgentEvent::TextDelta { delta, .. } if delta == "part"))
    })
    .await;
    rig.send(steer(3, "TYPED-C"));
    rig.wait_for(|msg| {
        matches!(
            msg,
            Msg::SteerAck {
                id: PendingId(3),
                result: Ok(())
            }
        )
    })
    .await;
    streaming.notify_one();
    llm.wait_call(3).await;
    assert!(!carried(&llm, 2, "TYPED-C"));
    assert!(carried(&llm, 3, "TYPED-C"), "{:?}", llm.requests()[3]);
    rig.wait_for(|msg| matches!(msg, Msg::SteerDelivered(PendingId(3))))
        .await;
    // (d) Two typed while request 3 is in flight.
    for (id, text) in [(4, "TYPED-D1"), (5, "TYPED-D2")] {
        rig.send(steer(id, text));
        rig.wait_for(move |msg| matches!(msg, Msg::SteerAck { id: PendingId(i), result: Ok(()) } if *i == id))
            .await;
    }
    in_flight.notify_one();
    llm.wait_call(5).await;
    assert!(carried(&llm, 4, "TYPED-D1") && !carried(&llm, 4, "TYPED-D2"));
    assert!(carried(&llm, 5, "TYPED-D2"), "{:?}", llm.requests()[5]);
    // (e) A note of ours (a helper's ending, say) comes while request 5 is
    // in flight, just before the user types: it is held, and rides with
    // the user's message, which rides request 6.
    let agent = rig.agent().await;
    let local = rig.daemon.local();
    let note = |text: &'static str| {
        let (local, agent) = (Arc::clone(&local), agent.clone());
        tokio::spawn(async move { local.steer_for_test(&agent, text).await })
    };
    let note_e = note("NOTE-E");
    common::until(|| local.held_notes_for_test(&agent) == 1).await;
    rig.send(steer(6, "TYPED-E"));
    rig.wait_for(|msg| {
        matches!(
            msg,
            Msg::SteerAck {
                id: PendingId(6),
                result: Ok(())
            }
        )
    })
    .await;
    assert_eq!(
        local.held_notes_for_test(&agent),
        0,
        "it went with the message"
    );
    in_flight.notify_one();
    llm.wait_call(6).await;
    assert!(carried(&llm, 6, "TYPED-E") && carried(&llm, 6, "NOTE-E"));
    note_e.await.unwrap().unwrap();
    // (g) Two notes held while request 6 is in flight, nobody typing: they
    // go as ONE steer once the call asked for tools (its review is held),
    // and ride request 7.
    let notes = [note("NOTE-G1"), note("NOTE-G2")];
    common::until(|| local.held_notes_for_test(&agent) == 2).await;
    in_flight.notify_one();
    for note in notes {
        note.await.unwrap().unwrap();
    }
    // The call's tool waits for its review meanwhile.
    common::until(|| classifier.seen().len() == 2).await;
    review.notify_one();
    llm.wait_call(7).await;
    let added = |at: usize| {
        let requests = llm.requests();
        user_texts(&requests[at])[user_texts(&requests[at - 1]).len()..].to_vec()
    };
    let both = added(7);
    assert!(
        both.len() == 1 && both[0].contains("NOTE-G1") && both[0].contains("NOTE-G2"),
        "{both:?}"
    );
    // (f) Request 7's context crosses the prepare threshold (read when its
    // tools are done): the prepare request is held while request 8 is in
    // flight; the user types then, and the message rides request 9 with it.
    rig.saw(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("asked the agent to prepare")))
        .await;
    llm.wait_call(8).await;
    assert_eq!(local.held_notes_for_test(&agent), 1);
    rig.send(steer(7, "TYPED-F"));
    rig.wait_for(|msg| {
        matches!(
            msg,
            Msg::SteerAck {
                id: PendingId(7),
                result: Ok(())
            }
        )
    })
    .await;
    assert_eq!(
        local.held_notes_for_test(&agent),
        0,
        "it went with the message"
    );
    in_flight.notify_one();
    llm.wait_call(9).await;
    assert!(!carried(&llm, 8, PREPARE));
    assert!(carried(&llm, 9, "TYPED-F") && carried(&llm, 9, PREPARE));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "done");
    assert_eq!(llm.requests().len(), 10, "one run, no follow-up");
    rig.quit().await;
}

/// Where a steer goes after another was withdrawn (`rkat/recall`) from
/// the boundary it waited for; see [`a_recalled_steer_s_slot_is_not_reused`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// While the tools of request 0 are held: m1, recall m1, then m1+m2.
    RecallThenCombine,
    /// The same while request 1 is held in flight.
    RecallThenCombineInFlight,
    /// While the tools of request 0 are held: m1, then m2, both left.
    TwoLeftAlone,
    /// While the tools of request 0 are held: m1, m2, recall m2, then m3.
    RecallSecondThenThird,
}

/// One run of `probe` on a daemon of its own: each steer's marker and the
/// first model request whose messages hold it (`None`: none did).
async fn probe_slots(probe: Probe) -> Vec<(&'static str, Option<usize>)> {
    use rkat_tui::client::remote;
    use rkat_tui::daemon::protocol::{RecallParams, method};
    let ws = Workspace::new();
    let review = Arc::new(Notify::new());
    let in_flight = Arc::new(Notify::new());
    let unknown = meerkat_comms::Keypair::generate()
        .public_key()
        .to_peer_id()
        .to_string();
    let contacts = |id: &'static str| Step::Tool {
        id,
        name: "contacts",
        args: json!({}),
    };
    let first = if probe == Probe::RecallThenCombineInFlight {
        contacts("c0")
    } else {
        // Its review is held: its tools run while the steers come.
        Step::Tool {
            id: "s0",
            name: "send_message",
            args: json!({"peer_id": unknown, "body": "x", "handling_mode": "queue"}),
        }
    };
    let llm = ScriptedLlmClient::new(vec![
        first,
        Step::Gated(Arc::clone(&in_flight), Box::new(contacts("c1"))),
        contacts("c2"),
        contacts("c3"),
        contacts("c4"),
        Step::Text("done"),
    ]);
    let classifier = ScriptedClassifier::holding(Arc::clone(&review));
    let setup = Setup::default().classifier(Arc::clone(&classifier) as _);
    let daemon =
        rig::Running::start_with(&setup.command_line(&ws.root), setup.options(&ws.root, &llm))
            .await;
    let agent = daemon.connect_bound().await;
    let steer = |text: &'static str, key: &'static str| {
        let agent = &agent;
        async move {
            remote::send(agent, text, true, key)
                .await
                .unwrap()
                .expect("the daemon names the steer's input")
                .input
        }
    };
    let recall = |input| {
        let agent = &agent;
        async move {
            let params = RecallParams {
                input_ids: vec![input],
                held_only: false,
            };
            let answer = remote::call::<method::Recall>(agent, &params)
                .await
                .unwrap();
            assert_eq!(answer.recalled.len(), 1, "{probe:?}: recalled: {answer:?}");
        }
    };
    remote::send(&agent, "go", false, "k0").await.unwrap();
    if probe == Probe::RecallThenCombineInFlight {
        llm.wait_call(1).await;
    } else {
        common::until(|| !classifier.seen().is_empty()).await;
    }
    let markers: &[&'static str] = match probe {
        Probe::RecallThenCombine | Probe::RecallThenCombineInFlight => {
            let m1 = steer("SLOT-M1", "k1").await;
            recall(m1).await;
            steer("SLOT-M1 SLOT-M2-COMBINED", "k2").await;
            &["SLOT-M1", "SLOT-M2-COMBINED"]
        }
        Probe::TwoLeftAlone => {
            steer("SLOT-M1", "k1").await;
            steer("SLOT-M2", "k2").await;
            &["SLOT-M1", "SLOT-M2"]
        }
        Probe::RecallSecondThenThird => {
            steer("SLOT-M1", "k1").await;
            let m2 = steer("SLOT-M2", "k2").await;
            recall(m2).await;
            steer("SLOT-M3", "k3").await;
            &["SLOT-M1", "SLOT-M2", "SLOT-M3"]
        }
    };
    if probe == Probe::RecallThenCombineInFlight {
        in_flight.notify_one();
    } else {
        review.notify_one();
        llm.wait_call(1).await;
        in_flight.notify_one();
    }
    llm.wait_call(5).await;
    let requests = llm.requests();
    let found = markers
        .iter()
        .map(|marker| {
            let at = requests
                .iter()
                .position(|messages| format!("{messages:?}").contains(marker));
            (*marker, at)
        })
        .collect();
    daemon.stop().await;
    found
}

/// The owner's question: can a withdrawn steer be replaced in the
/// boundary slot it held? Measured on the engine, each
/// probe on a daemon of its own (every step waits on the send's or the
/// recall's answer, no sleep; six rounds of each gave the same outcome
/// when it was measured): (1) a steer recalled while
/// the tools of request 0 run, and a combined one admitted after it; (2)
/// the same while request 1 is in flight; (3) the control, two steers left
/// alone, one per request; (4) of two waiting steers the second recalled
/// and a third admitted. A steer recalled from the front of the queue
/// leaves its request empty: what is admitted after it rides the request
/// after (N+2), so recalling a waiting message to resend it combined with
/// a new one costs a request. One recalled behind another loses nothing.
/// The outcomes are `docs/evidence.md`'s ("A recalled steer's slot").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recalled_steer_s_slot_is_not_reused() {
    // The recalled m1 rides nowhere: its marker shows first inside the
    // combined steer.
    let expected: [(Probe, &[(&str, Option<usize>)]); 4] = [
        // Request 1 carries no steer: the combined one rides request 2.
        (
            Probe::RecallThenCombine,
            &[("SLOT-M1", Some(2)), ("SLOT-M2-COMBINED", Some(2))],
        ),
        // Request 2 carries none: it rides request 3.
        (
            Probe::RecallThenCombineInFlight,
            &[("SLOT-M1", Some(3)), ("SLOT-M2-COMBINED", Some(3))],
        ),
        (
            Probe::TwoLeftAlone,
            &[("SLOT-M1", Some(1)), ("SLOT-M2", Some(2))],
        ),
        // Behind a waiting steer nothing is lost: m3 rides the request
        // after m1's.
        (
            Probe::RecallSecondThenThird,
            &[
                ("SLOT-M1", Some(1)),
                ("SLOT-M2", None),
                ("SLOT-M3", Some(2)),
            ],
        ),
    ];
    for (probe, want) in expected {
        let outcome = probe_slots(probe).await;
        assert_eq!(outcome.as_slice(), want, "{probe:?}");
    }
}

/// The prepare instruction of [`a_typed_message_rides_the_first_request_after_it`].
const PREPARE: &str = "PREPARE-NOW";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steered_message_is_applied_at_the_next_tool_boundary() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("notes.txt"), "the answer is 42\n").unwrap();
    let gate = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(
            Arc::clone(&gate),
            Box::new(Step::Tool {
                id: "call-1",
                name: "read_file",
                args: json!({ "path": "notes.txt" }),
            }),
        ),
        Step::Text("done"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("read the notes".into()));
    llm.wait_call(0).await;
    // Typed while the model streams: steered into the running turn.
    rig.send(steer(1, "also mention the file name"));
    let ack = rig
        .wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    assert!(
        matches!(
            ack,
            Msg::SteerAck {
                id: PendingId(1),
                result: Ok(())
            }
        ),
        "{ack:?}"
    );
    gate.notify_one();

    rig.wait_for(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))))
        .await;
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "done");

    // One run: the steer joined it at the tool boundary, after the tool
    // result (the file tool ran end to end) and before the next request.
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "no follow-up turn");
    let second = &requests[1];
    let tool_result_at = second
        .iter()
        .position(|m| {
            matches!(m, Message::ToolResults { results, .. }
            if results.iter().any(|r| r.text_content().contains("the answer is 42")))
        })
        .expect("the read_file result reached the model");
    let steer_at = second
        .iter()
        .position(
            |m| matches!(m, Message::User(u) if u.text_content() == "also mention the file name"),
        )
        .expect("the steered message reached the model");
    assert!(steer_at > tool_result_at);
    assert!(
        rig.events()
            .iter()
            .any(|event| matches!(event, AgentEvent::BoundaryAppendApplied { .. }))
    );
    let session = rig.messages().await;
    assert_eq!(
        user_texts(&session),
        vec![
            "read the notes".to_string(),
            "also mention the file name".to_string()
        ]
    );
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continue_and_resume_by_id_across_lifetimes() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("remembered")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let first = rig.current_session().await;
    rig.send(Effect::StartTurn("remember me".into()));
    let _ = rig.turn_finished().await;
    rig.quit().await;

    // `-c` with a restarted daemon: the conversation it restored, with its
    // transcript.
    let llm = ScriptedLlmClient::new(vec![Step::Text("continued")]);
    let mut rig = Attached::start_with(&ws.root, &llm, client_args(&["-c"])).await;
    assert_eq!(rig.current_session().await, first);
    let blocks = match rig
        .wait_for(|msg| matches!(msg, Msg::ResetTranscript(_)))
        .await
    {
        Msg::ResetTranscript(blocks) => blocks,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        blocks,
        vec![
            Block::User {
                text: "remember me".into()
            },
            Block::Assistant {
                md: "remembered".into()
            },
        ]
    );
    rig.send(Effect::StartTurn("still there?".into()));
    let _ = rig.turn_finished().await;
    assert_eq!(
        user_texts(&llm.requests()[0]),
        vec!["remember me".to_string(), "still there?".to_string()]
    );
    rig.quit().await;

    // `--resume <id prefix>` of the current conversation: attached to the
    // agent running it, with no switch.
    let prefix = first.to_string()[..30].to_string();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&ws.root, &llm, client_args(&["--resume", &prefix])).await;
    let shown = first.to_string();
    rig.saw(|m| matches!(m, Msg::Status(rkat_tui::ui::StatusUpdate::Session(id)) if *id == shown))
        .await;
    assert!(
        !rig.notices().contains("resumed conversation"),
        "{}",
        rig.notices()
    );
    assert_eq!(rig.current_session().await, first);
    rig.quit().await;
}

/// `--resume` without a reference: the picker offers the earlier
/// conversation first thing; choosing it resumes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_without_a_reference_opens_the_picker_at_start() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let first = rig.current_session().await;
    rig.send(Effect::StartTurn("pick me later".into()));
    let _ = rig.turn_finished().await;
    // Another conversation is the current one when the picker opens.
    rig.send(Effect::NewSession);
    rig.wait_for(|m| matches!(m, Msg::ResetTranscript(blocks) if blocks.is_empty()))
        .await;
    rig.quit().await;

    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&ws.root, &llm, client_args(&["--resume"])).await;
    assert_ne!(rig.current_session().await, first);
    let items = match rig
        .wait_for(|msg| matches!(msg, Msg::ShowResumePicker(_)))
        .await
    {
        Msg::ShowResumePicker(items) => items,
        other => panic!("{other:?}"),
    };
    let first_id = first.to_string();
    let chosen = items
        .iter()
        .find(|(reference, _)| reference == &first_id)
        .map(|(reference, _)| reference.clone())
        .unwrap_or_else(|| panic!("the earlier session is offered: {items:?}"));
    rig.send(Effect::ResumeSession(chosen));
    let blocks = match rig
        .wait_for(|msg| matches!(msg, Msg::ResetTranscript(blocks) if !blocks.is_empty()))
        .await
    {
        Msg::ResetTranscript(blocks) => blocks,
        other => panic!("{other:?}"),
    };
    assert!(blocks.contains(&Block::User {
        text: "pick me later".into()
    }));
    rig.quit().await;
}

/// Move the workspace `ws` (its daemon stopped) to the sibling `name`:
/// copied, then removed.
async fn move_workspace(ws: &Workspace, name: &str) -> std::path::PathBuf {
    let moved = ws.sibling(name);
    copy_tree(&ws.root, &moved);
    // A user moves the directory after rkat-tui exited; here the process
    // lives on, and a file operation the service's shutdown left in flight
    // can still land for a moment, so the removal gets three tries. A
    // leftover that persists is reported by name.
    let mut tries = 0;
    loop {
        match std::fs::remove_dir_all(&ws.root) {
            Ok(()) => break,
            Err(err) if tries < 3 => {
                tries += 1;
                eprintln!(
                    "remove_dir_all try {tries}: {err}; present: {:?}",
                    remaining_entries(&ws.root)
                );
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(err) => panic!(
                "remove_dir_all: {err}; left behind: {:#?}",
                remaining_entries(&ws.root)
            ),
        }
    }
    moved
}

/// A conversation in `ws`, then the daemon stopped: its session.
async fn one_conversation(ws: &Workspace, prompt: &str) -> meerkat_core::SessionId {
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let first = rig.current_session().await;
    rig.send(Effect::StartTurn(prompt.into()));
    let _ = rig.turn_finished().await;
    rig.quit().await;
    first
}

/// The workspaces' realms of the rig's config directory beside `root`.
fn realms(root: &std::path::Path) -> std::path::PathBuf {
    common::config_dir_for(root).join(realm_home::WORKSPACES_DIR)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_moved_workspace_starts_anew_and_is_offered_its_old_state() {
    let ws = Workspace::new();
    let first = one_conversation(&ws, "before the move").await;
    let moved = move_workspace(&ws, "renamed-project").await;
    // The realm stays behind, unclaimed: the manifest names the old path,
    // and nothing in the moved workspace says otherwise. A start there
    // gets a new realm and is offered the old one; nothing is adopted.
    let old = realm_home::find(&realms(&moved), &ws.root)
        .unwrap()
        .unwrap();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&moved, &llm, client_args(&["-c"])).await;
    assert_ne!(rig.current_session().await, first);
    rig.wait_for(|msg| {
        matches!(msg, Msg::Notice { text, .. } if text.contains(&old.id) && text.contains("/rehome"))
    })
    .await;
    rig.quit().await;
    assert_eq!(
        realm_home::find(&realms(&moved), &ws.root).unwrap(),
        Some(old)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_without_a_running_turn_is_refused() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    // A steer without a running turn is refused so the UI sends it as a turn.
    rig.send(steer(7, "idle"));
    let msg = rig
        .wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    assert!(
        matches!(
            msg,
            Msg::SteerAck {
                id: PendingId(7),
                result: Err(rkat_tui::ui::delivery::SteerFailure::NotRunning)
            }
        ),
        "{msg:?}"
    );
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_builtins_the_tui_owns_are_denied_by_the_tool_policy() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "swap",
            name: "brain_swap",
            args: json!({ "model": "claude-opus-4-8" }),
        },
        Step::Tool {
            id: "patch",
            name: "apply_patch",
            args: json!({ "input": "*** Begin Patch\n*** End Patch\n" }),
        },
        Step::Text("done"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("try them".into()));
    let _ = rig.turn_finished().await;
    let requests = llm.requests();
    let result_of = |id: &str| {
        requests
            .iter()
            .flatten()
            .find_map(|m| match m {
                Message::ToolResults { results, .. } => {
                    results.iter().find(|r| r.tool_use_id == id).cloned()
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no result for {id}"))
    };
    for id in ["swap", "patch"] {
        let result = result_of(id);
        assert!(result.is_error, "{id}: {result:?}");
    }
    // The model never changed.
    let id = rig.current_session().await;
    rig.quit().await;
    let meta = rig::stored_session(&ws.root, &id)
        .await
        .session_metadata()
        .unwrap();
    assert_eq!(meta.model, common::MODEL);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_prompt_after_a_failed_turn_is_not_added_twice() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::RejectTools, Step::Text("fixed")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("fix the build".into()));
    let outcome = rig.turn_finished().await;
    assert!(
        matches!(outcome, Err(TurnFailure::Failed(_))),
        "{outcome:?}"
    );
    // The failed run committed the prompt.
    let session = rig.messages().await;
    assert_eq!(user_texts(&session), vec!["fix the build".to_string()]);
    // What the retry is compared with travels alone: the
    // last message, the prompt, not the transcript.
    let id = rig.current_session().await;
    let last = rig
        .daemon
        .connect()
        .await
        .call("rkat/last_message", json!({"session_id": id.to_string()}))
        .await
        .unwrap();
    let last: Vec<Message> = serde_json::from_value(last["messages"].clone()).unwrap();
    assert_eq!(user_texts(&last), vec!["fix the build".to_string()]);
    assert_eq!(last.len(), 1, "{last:?}");

    // Re-typed (with stray whitespace): a retry, not a second copy.
    rig.send(Effect::StartTurn("  fix the build\n".into()));
    rig.wait_for(
        |msg| matches!(msg, Msg::Notice { text, .. } if text.contains("already in the transcript")),
    )
    .await;
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("retry succeeds").text, "fixed");
    let expected = vec![
        "fix the build".to_string(),
        "(retrying the request above)".to_string(),
    ];
    assert_eq!(user_texts(&llm.requests()[1]), expected);
    let session = rig.messages().await;
    assert_eq!(user_texts(&session), expected);
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_different_prompt_after_a_failed_turn_is_sent_as_written() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::RejectTools,
        Step::Text("first"),
        Step::Text("second"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("fix the build".into()));
    assert!(rig.turn_finished().await.is_err());

    rig.send(Effect::StartTurn("run the tests instead".into()));
    assert_eq!(rig.turn_finished().await.expect("turn").text, "first");
    // Only right after the failure: a repeat after a success is a message.
    rig.send(Effect::StartTurn("run the tests instead".into()));
    assert_eq!(rig.turn_finished().await.expect("turn").text, "second");
    let session = rig.messages().await;
    assert_eq!(
        user_texts(&session),
        vec![
            "fix the build".to_string(),
            "run the tests instead".to_string(),
            "run the tests instead".to_string(),
        ]
    );
    rig.quit().await;
}

/// Allows every reviewed call (the default classifier of the rig): the
/// slow-tool cases are about delivery, not the approval gate.
fn allow_shell() -> Setup {
    Setup::default()
}

/// The dogfood case: a message typed while the model's shell call runs
/// reaches the model after that call's result, and the call runs to its end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_typed_while_a_slow_tool_runs_reaches_the_model_after_its_result() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "slow",
            name: "shell",
            args: json!({ "command": "sleep 2; echo done sleeping", "timeout_secs": 30 }),
        },
        Step::Text("finished"),
        Step::Text("follow-up answer"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, allow_shell()).await;
    rig.send(Effect::StartTurn("do something for a while".into()));
    tool_started(&mut rig, "slow").await;
    rig.send(steer(1, "typed while it sleeps"));
    let ack = rig
        .wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    assert!(
        matches!(ack, Msg::SteerAck { result: Ok(()), .. }),
        "{ack:?}"
    );

    rig.wait_for(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))))
        .await;
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "finished");
    // One run: the shell ran to its end, its result reached the model, and
    // the typed message followed it in the same request.
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "no follow-up turn");
    let second = &requests[1];
    let result_at = second
        .iter()
        .position(|m| {
            matches!(m, Message::ToolResults { results, .. }
            if results.iter().any(|r| !r.is_error && r.text_content().contains("done sleeping")))
        })
        .expect("the shell result reached the model");
    let typed_at = second
        .iter()
        .position(|m| matches!(m, Message::User(u) if u.text_content() == "typed while it sleeps"))
        .expect("the typed message reached the model");
    assert!(typed_at > result_at);
    let session = rig.messages().await;
    assert!(
        !session
            .iter()
            .any(|m| matches!(m, Message::SystemNotice(_))),
        "nothing was closed by recovery"
    );
    rig.quit().await;
}

/// ↑ takes back a message typed during a slow tool while the runtime still
/// holds it queued (`rkat/recall`, by the input the send admitted): it never
/// reaches the model, and the turn runs on: the tool ends, its result
/// reaches the model, the turn completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recalling_a_message_typed_during_a_slow_tool_leaves_the_run_alone() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "slow",
            name: "shell",
            args: json!({ "command": "sleep 2; echo done sleeping", "timeout_secs": 30 }),
        },
        Step::Text("finished"),
        Step::Text("follow-up answer"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, allow_shell()).await;
    rig.send(Effect::StartTurn("do something for a while".into()));
    tool_started(&mut rig, "slow").await;
    rig.send(steer(1, "never mind"));
    let ack = rig
        .wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    assert!(
        matches!(ack, Msg::SteerAck { result: Ok(()), .. }),
        "{ack:?}"
    );
    rig.send(Effect::RecallQueued);
    let recalled = rig
        .wait_for(|msg| matches!(msg, Msg::Recalled { .. }))
        .await;
    assert!(
        matches!(&recalled, Msg::Recalled { recalled, errors }
            if recalled == &[PendingId(1)] && errors.is_empty()),
        "{recalled:?}"
    );

    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "finished");
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "no follow-up turn");
    assert!(
        requests[1]
            .iter()
            .any(|m| matches!(m, Message::ToolResults { results, .. }
        if results.iter().any(|r| !r.is_error && r.text_content().contains("done sleeping"))))
    );
    for request in &requests {
        assert!(!user_texts(request).contains(&"never mind".to_string()));
    }
    let session = rig.messages().await;
    assert!(!user_texts(&session).contains(&"never mind".to_string()));
    assert!(
        !session
            .iter()
            .any(|m| matches!(m, Message::SystemNotice(_))),
        "nothing was closed by recovery"
    );
    rig.quit().await;
}

/// A recall after the steer joined the run finds nothing to pull back: the
/// daemon reports it reached, cancels nothing, and the turn runs on with
/// the steer delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recalling_a_steer_that_already_joined_the_run_leaves_it_delivered() {
    let ws = Workspace::new();
    let gate = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "slow",
            name: "shell",
            args: json!({ "command": "sleep 1; echo done sleeping", "timeout_secs": 30 }),
        },
        Step::Gated(Arc::clone(&gate), Box::new(Step::Text("finished"))),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, allow_shell()).await;
    rig.send(Effect::StartTurn("do something for a while".into()));
    tool_started(&mut rig, "slow").await;
    rig.send(steer(1, "typed while it sleeps"));
    rig.wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    rig.wait_for(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))))
        .await;
    llm.wait_call(1).await;
    rig.send(Effect::RecallQueued);
    let recalled = rig
        .wait_for(|msg| matches!(msg, Msg::Recalled { .. }))
        .await;
    assert!(
        matches!(&recalled, Msg::Recalled { recalled, errors }
            if recalled.is_empty() && errors.is_empty()),
        "{recalled:?}"
    );
    gate.notify_one();

    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "finished");
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "no follow-up turn");
    assert!(user_texts(&requests[1]).contains(&"typed while it sleeps".to_string()));
    rig.quit().await;
}

/// A call the classifier allowed gets a dim note under its tool block with
/// the classifier's reason; a tier-1 call gets none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_classifier_allowed_call_gets_a_note_and_a_tier1_call_does_not() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("scratch.txt"), "x").unwrap();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tools(vec![
            ("tier1", "shell", json!({ "command": "ls" })),
            (
                "reviewed",
                "shell",
                json!({ "command": "rm -rf scratch.txt" }),
            ),
        ]),
        Step::Text("cleaned up"),
    ]);
    let classifier = ScriptedClassifier::allowing(vec![ClassifierVerdict::allow(
        "removes an in-project scratch file\nthe user asked to clean up",
    )]);
    let options = Setup::default().classifier(classifier as Arc<dyn ActionClassifier>);
    let mut rig = Attached::start_with(&ws.root, &llm, options).await;
    rig.send(Effect::StartTurn("clean up the scratch file".into()));
    let note = rig
        .wait_for(
            |msg| matches!(msg, Msg::ToolNote { tool_use_id, .. } if tool_use_id == "reviewed"),
        )
        .await;
    let Msg::ToolNote { text, .. } = note else {
        panic!("not a note: {note:?}");
    };
    assert_eq!(
        text,
        "allowed by classifier: removes an in-project scratch file the user asked to clean up"
    );
    assert_eq!(rig.turn_finished().await.expect("turn").text, "cleaned up");
    assert!(
        !ws.root.join("scratch.txt").exists(),
        "the reviewed call ran"
    );
    let notes: Vec<&Msg> = rig
        .seen
        .iter()
        .filter(|msg| matches!(msg, Msg::ToolNote { .. }))
        .collect();
    assert_eq!(notes.len(), 1, "only the reviewed call is noted: {notes:?}");
    // The classifier allow no longer logs a separate approval line.
    assert!(!rig.seen.iter().any(|msg| matches!(msg, Msg::Approval(_))));
    rig.quit().await;
}

/// The model writes a protected `.envrc`: even with a classifier that
/// allows everything, the UI gets an escalation naming the protected path;
/// answering allow runs the write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_protected_write_is_put_to_the_user_and_runs_when_allowed() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "env",
            name: "write_file",
            args: json!({ "path": ".envrc", "content": "API_URL=http://localhost\n" }),
        },
        Step::Text("configured"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, allow_shell()).await;
    rig.send(Effect::StartTurn("add a test env file".into()));
    let Msg::Escalation(prompt, id) = rig.wait_for(|msg| matches!(msg, Msg::Escalation(..))).await
    else {
        unreachable!()
    };
    assert_eq!(prompt.tool, "write_file");
    assert!(
        prompt
            .policy_reason
            .as_deref()
            .is_some_and(|r| r.contains("protected path: `.envrc` (secrets)")),
        "{prompt:?}"
    );
    rig.send(Effect::RespondEscalation(id, EscalationAnswer::AllowOnce));
    assert_eq!(rig.turn_finished().await.expect("turn").text, "configured");
    assert_eq!(
        std::fs::read_to_string(ws.root.join(".envrc")).unwrap(),
        "API_URL=http://localhost\n"
    );
    assert!(
        !rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::ToolNote { .. })),
        "no classifier note"
    );
    rig.quit().await;
}

/// The texts no interrupt may show the user (the engine's own words).
const RAW_INTERRUPT_TEXTS: &[&str] = &[
    "Run was cancelled",
    "MeerkatMachine",
    "Runtime not ready",
    "no live session actor",
    "could not be read again",
    "interrupt incomplete",
    "from the approval dialog",
];

/// The owner's report, in its order: the agent inside a
/// long foreground shell call, a message typed meanwhile (admitted as a
/// steer, waiting for the tool), then Esc three times. Before the fix the
/// first press stopped the run, the runtime ran the typed message as a
/// follow-up run, and the later presses (force, the 10 s hard cancel) hit
/// that run. Now the first press stops the run once, by its id, and takes
/// the message back: no run starts from it, it is back in the editor, the
/// later presses act on nothing and say so once, and the transcript holds
/// none of the engine's words, only the one line.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_presses_during_a_slow_shell_stop_one_run_and_return_the_typed_message() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "slow",
            name: "shell",
            args: json!({ "command": "sleep 30; echo done", "timeout_secs": 60 }),
        },
        Step::Text("a run started from the typed message"),
        Step::Text("spare"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, allow_shell()).await;
    let agent = rig.agent().await;
    let mut app = rkat_tui::ui::App::new(rkat_tui::ui::StatusInfo::default());
    for msg in rig.seen.clone() {
        for effect in app.update(msg) {
            rig.send(effect);
        }
    }
    type_into(&rig, &mut app, "run something long");
    pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::Agent(env)
            if matches!(&env.payload, AgentEvent::ToolExecutionStarted { id, .. } if id == "slow"))
    })
    .await;
    type_into(&rig, &mut app, "how is it going");
    pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::SteerAck { result: Ok(()), .. })
    })
    .await;
    let esc = |app: &mut rkat_tui::ui::App, rig: &Attached| {
        for effect in app.update(Msg::Key(KeyEvent::from(KeyCode::Esc))) {
            rig.send(effect);
        }
    };
    // Press 1, then (ordered on the run's end, not on time) presses 2 and 3.
    esc(&mut app, &rig);
    let ended = pump(
        &mut rig,
        &mut app,
        |msg| matches!(msg, Msg::Agent(env) if matches!(env.payload, AgentEvent::RunFailed { .. })),
    )
    .await;
    let Msg::Agent(ended) = ended else {
        unreachable!()
    };
    let AgentEvent::RunFailed { identity, .. } = &ended.payload else {
        unreachable!()
    };
    let first = identity.run_id.clone().expect("the run's id");
    esc(&mut app, &rig);
    esc(&mut app, &rig);
    pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::TurnFinished(_))
    })
    .await;
    // A sync point after the presses: the runner answered every effect
    // before it.
    rig.send(Effect::RecallQueued);
    pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::Recalled { .. })
    })
    .await;
    rig.daemon.idle(&agent).await;

    // The daemon stopped one run, the first, once; no engine interrupt
    // method was sent at all.
    let interrupts = rig.daemon.local().interrupts();
    assert!(!interrupts.is_empty());
    assert!(
        interrupts.iter().all(|i| i.run_id.as_ref() == Some(&first)),
        "{interrupts:#?}"
    );
    assert_eq!(
        interrupts
            .iter()
            .filter(|i| i.run == rkat_tui::daemon::protocol::RunInterrupt::Stopped)
            .count(),
        1,
        "{interrupts:#?}"
    );
    for method in [
        "mob/stop_member_run",
        "mob/force_cancel",
        "mob/hard_cancel_member",
    ] {
        assert_eq!(rig.daemon.forwarded(method), 0, "{method}");
    }
    // No run started from the typed message; it is back in the editor.
    assert_eq!(llm.requests().len(), 1, "no follow-up run");
    let session = rig.messages().await;
    assert!(!user_texts(&session).contains(&"how is it going".to_string()));
    assert!(
        rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::Returned(ids) if ids == &[PendingId(1)])),
        "the message comes back: {interrupts:#?}; told instead: {:?}",
        rig.seen
            .iter()
            .filter(|msg| matches!(msg, Msg::NotDelivered { .. } | Msg::Returned(_)))
            .collect::<Vec<_>>()
    );
    assert!(
        !rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))))
    );
    assert_eq!(app.input.text(), "how is it going");
    assert!(!app.is_running());
    // The presses after the run's end said so once.
    let already = rig
        .seen
        .iter()
        .filter(|msg| matches!(msg, Msg::Notice { text, .. } if text == "the turn is already stopped; nothing else was interrupted"))
        .count();
    assert_eq!(already, 1, "{:#?}", rig.seen);
    // The transcript: the one line, none of the engine's words.
    let text = app.transcript.export_text();
    for raw in RAW_INTERRUPT_TEXTS {
        assert!(!text.contains(raw), "{raw}: {text}");
    }
    assert!(
        text.contains(
            "turn interrupted; the agent is idle, and your typed message is back in the input"
        ),
        "{text}"
    );
    assert_eq!(text.matches("turn interrupted").count(), 1, "{text}");
    rig.quit().await;
}

/// Ctrl-X Ctrl-S on an empty editor while a message typed during a long
/// tool call waits ("delivering…"): the run is interrupted, the message
/// comes back from the agent and goes at once as the next run's prompt,
/// not to the editor: the agent's next model request holds it once, the
/// editor is empty, and no other run starts for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_now_pushes_a_message_held_behind_a_long_tool_call_as_the_next_run() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "slow",
            name: "shell",
            args: json!({ "command": "sleep 30; echo done", "timeout_secs": 60 }),
        },
        Step::Text("answered the pushed message"),
        Step::Text("a second run"),
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, allow_shell()).await;
    let agent = rig.agent().await;
    let mut app = rkat_tui::ui::App::new(rkat_tui::ui::StatusInfo::default());
    for msg in rig.seen.clone() {
        for effect in app.update(msg) {
            rig.send(effect);
        }
    }
    type_into(&rig, &mut app, "run something long");
    pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::Agent(env)
            if matches!(&env.payload, AgentEvent::ToolExecutionStarted { id, .. } if id == "slow"))
    })
    .await;
    type_into(&rig, &mut app, "how is it going");
    pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::SteerAck { result: Ok(()), .. })
    })
    .await;
    assert!(app.input.is_empty());
    for c in ['x', 's'] {
        for effect in app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::CONTROL,
        ))) {
            rig.send(effect);
        }
    }
    let interrupted = pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::TurnFinished(_))
    })
    .await;
    assert!(
        matches!(
            interrupted,
            Msg::TurnFinished(Err(TurnFailure::Interrupted))
        ),
        "{interrupted:?}"
    );
    // The app started the next run with the message itself.
    assert!(app.is_running());
    assert!(app.input.is_empty(), "{:?}", app.input.text());
    let answered = pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::TurnFinished(_))
    })
    .await;
    assert!(
        matches!(&answered, Msg::TurnFinished(Ok(done)) if done.text == "answered the pushed message"),
        "{answered:?}"
    );
    rig.daemon.idle(&agent).await;
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "no second run for it");
    assert_eq!(
        user_texts(&requests[1])
            .iter()
            .filter(|text| text.contains("how is it going"))
            .count(),
        1,
        "{:#?}",
        requests[1]
    );
    let session = rig.messages().await;
    assert_eq!(count(&session, "how is it going"), 1, "{session:#?}");
    assert!(app.input.is_empty());
    let text = app.transcript.export_text();
    assert!(
        text.contains("interrupted; sending your waiting message now"),
        "{text}"
    );
    assert_eq!(text.matches("interrupted;").count(), 1, "{text}");
    rig.quit().await;
}

/// A single Esc still stops the run at once, by its id, and a message the
/// agent already read before the interrupt stays delivered (it is not
/// returned).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_esc_stops_the_run_by_its_id_and_keeps_a_message_already_read() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "short",
            name: "shell",
            args: json!({ "command": "sleep 1; echo quick", "timeout_secs": 30 }),
        },
        Step::Hang,
    ]);
    let mut rig = Attached::start_with(&ws.root, &llm, allow_shell()).await;
    rig.send(Effect::StartTurn("go".into()));
    let started = rig
        .wait_for(|msg| matches!(msg, Msg::Agent(env) if matches!(env.payload, AgentEvent::RunStarted { .. })))
        .await;
    let Msg::Agent(started) = started else {
        unreachable!()
    };
    let AgentEvent::RunStarted { identity, .. } = &started.payload else {
        unreachable!()
    };
    let run = identity.run_id.clone().expect("the run's id");
    tool_started(&mut rig, "short").await;
    rig.send(steer(1, "read before the interrupt"));
    // It joins the run at the tool's boundary; the model's next request
    // (held open) carries it.
    rig.wait_for(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))))
        .await;
    llm.wait_call(1).await;
    assert!(user_texts(&llm.requests()[1]).contains(&"read before the interrupt".to_string()));
    let asked = std::time::Instant::now();
    rig.send(Effect::Interrupt);
    assert_eq!(rig.turn_finished().await, Err(TurnFailure::Interrupted));
    assert!(asked.elapsed() < rkat_tui::client::runner::INTERRUPT_DEADLINE / 2);
    let interrupts = rig.daemon.local().interrupts();
    assert_eq!(interrupts.len(), 1, "{interrupts:#?}");
    assert_eq!(interrupts[0].run_id.as_ref(), Some(&run));
    assert_eq!(
        interrupts[0].run,
        rkat_tui::daemon::protocol::RunInterrupt::Stopped
    );
    assert!(interrupts[0].returned.is_empty(), "{interrupts:#?}");
    assert!(!rig.seen.iter().any(|msg| matches!(msg, Msg::Returned(_))));
    rig.quit().await;
}

/// Force still works when the run does not end: `rkat/interrupt` with
/// `hard` cancels the stalled run at once, by its id; and one naming a
/// run that has ended stops nothing, however the agent runs now.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forced_interrupt_cancels_its_run_and_never_a_later_one() {
    use rkat_tui::daemon::protocol::{Interrupted, RunInterrupt};
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Hang, Step::Hang, Step::Text("after")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("stall".into()));
    let started = rig
        .wait_for(|msg| matches!(msg, Msg::Agent(env) if matches!(env.payload, AgentEvent::RunStarted { .. })))
        .await;
    let Msg::Agent(started) = started else {
        unreachable!()
    };
    let AgentEvent::RunStarted { identity, .. } = &started.payload else {
        unreachable!()
    };
    let first = identity.run_id.clone().expect("the run's id");
    llm.wait_call(0).await;
    let other = rig.daemon.connect_bound().await;
    let forced: Interrupted = serde_json::from_value(
        other
            .call(
                "rkat/interrupt",
                json!({"target": "run", "run_id": first, "hard": true}),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(forced.run, RunInterrupt::Stopped, "{forced:?}");
    assert_eq!(rig.turn_finished().await, Err(TurnFailure::Interrupted));
    // A later run, stalled too: the old run's interrupt leaves it alone.
    rig.send(Effect::StartTurn("stall again".into()));
    llm.wait_call(1).await;
    for hard in [false, true] {
        let late: Interrupted = serde_json::from_value(
            other
                .call(
                    "rkat/interrupt",
                    json!({"target": "run", "run_id": first, "hard": hard}),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(late.run, RunInterrupt::Ended, "{late:?}");
    }
    assert!(
        rig.daemon
            .status()
            .await
            .agents
            .iter()
            .any(|row| row.running == Some(true))
    );
    rig.send(Effect::Interrupt);
    assert_eq!(rig.turn_finished().await, Err(TurnFailure::Interrupted));
    rig.quit().await;
}

/// Waits until the tool call `id` has started executing.
async fn tool_started(rig: &mut Attached, id: &str) {
    rig.saw(|msg| {
        matches!(msg, Msg::Agent(env)
            if matches!(&env.payload, AgentEvent::ToolExecutionStarted { id: started, .. } if started == id))
    })
    .await;
}

/// Feed the runner's messages to `app` (forwarding the effects it answers
/// with) until one matches `until`; returns that message.
async fn pump(
    rig: &mut Attached,
    app: &mut rkat_tui::ui::App,
    until: impl Fn(&Msg) -> bool,
) -> Msg {
    loop {
        let msg = rig.wait_for(|_| true).await;
        let stop = until(&msg);
        for effect in app.update(msg.clone()) {
            rig.send(effect);
        }
        if stop {
            return msg;
        }
    }
}

/// `/context`: the runner adds the session's system prompt, its messages,
/// the instruction files and the TUI's own tools to the UI's header.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_report_describes_the_session() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("AGENTS.md"), "# AGENTS.md\n\n- use tabs\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("hello")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("hi".into()));
    let _ = rig.turn_finished().await;
    // The daemon reads the tool definitions once the agent is idle.
    let session = rig.current_session().await;
    common::until(|| rig.daemon.origins.sizes(&session).is_some()).await;
    rig.send(Effect::ContextReport("context: header".into()));
    let msg = rig
        .wait_for(
            |msg| matches!(msg, Msg::Notice { text, .. } if text.starts_with("context: header")),
        )
        .await;
    let Msg::Notice { text, .. } = msg else {
        unreachable!()
    };
    assert!(text.contains("system prompt: "), "{text}");
    assert!(text.contains("messages: 2 (user 1, assistant 1"), "{text}");
    assert!(text.contains("workspace AGENTS.md: "), "{text}");
    assert!(text.contains("24 bytes"), "{text}");
    assert!(
        text.contains("tool definitions (every model request carries them): "),
        "{text}"
    );
    for family in [
        "schedule: 8 tools",
        "mob: ",
        "comms: ",
        "files: ",
        "notes: ",
    ] {
        assert!(text.contains(family), "{family}: {text}");
    }
    rig.quit().await;
}

/// `/status`: one notice naming the version, model, session and the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_report_names_the_session_and_the_workspace() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StatusReport);
    let msg = rig
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.starts_with("rkat-tui ")))
        .await;
    let Msg::Notice { text, .. } = msg else {
        unreachable!()
    };
    assert!(
        text.contains(&format!("conversation: {}", rig.current_session().await)),
        "{text}"
    );
    // The daemon's address, the agent and the conversation (full and
    // short) each on a labelled line: the status bar leaves them out.
    let session = rig.current_session().await.to_string();
    assert!(
        text.contains(&format!(
            "\nconversation: {session} (short {})\n",
            rkat_tui::text::short_id(&session)
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("\nagent: {}\n", rig.agent().await)),
        "{text}"
    );
    assert!(text.contains("\ndaemon: 127.0.0.1:"), "{text}");
    assert!(
        text.contains(&format!("workspace: {}", ws.root.display())),
        "{text}"
    );
    assert!(text.contains("attached to the workspace daemon"), "{text}");
    assert!(text.contains("approval gate: in the daemon"), "{text}");
    // The daemon's side (`rkat/report`): the realm, the shell's
    // confinement, the permission mode, the realm rules file and the
    // helpers' depth limit.
    let realm = rkat_tui::bootstrap::resolve_scope(&common::args_for(&ws.root))
        .unwrap()
        .realm_root();
    assert!(
        text.contains(&format!("realm: {}", realm.display())),
        "{text}"
    );
    assert!(text.contains("shell confinement: off"), "{text}");
    assert!(text.contains("permission mode: auto"), "{text}");
    assert!(
        text.contains(&format!(
            "realm rules: {}",
            realm.join("tui.toml").display()
        )),
        "{text}"
    );
    assert!(
        text.contains("helpers: depth limit on (max_depth 1"),
        "{text}"
    );
    rig.quit().await;
}

/// `/export`: the runner writes the UI's text to the default file in the
/// workspace's realm (and to a typed path), and says where.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_writes_the_transcript_and_names_the_file() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let id = rig.current_session().await;
    rig.send(Effect::Export {
        path: None,
        text: "## user\nhi\n".into(),
    });
    let msg = rig
        .wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("export")))
        .await;
    let Msg::Notice { text, .. } = msg else {
        unreachable!()
    };
    let path = text
        .strip_prefix("exported the transcript to ")
        .unwrap_or_else(|| panic!("{text}"));
    let path = std::path::Path::new(path);
    assert!(path.starts_with(common::realm_of(&ws.root)), "{text}");
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with(&format!("export-{id}-")) && name.ends_with(".txt"),
        "{name}"
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "## user\nhi\n");

    rig.send(Effect::Export {
        path: Some("out/chat.txt".into()),
        text: "x\n".into(),
    });
    rig.wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("chat.txt")))
        .await;
    assert_eq!(
        std::fs::read_to_string(ws.root.join("out/chat.txt")).unwrap(),
        "x\n"
    );
    rig.quit().await;
}

/// The runner's quit releases the lease the bootstrap took.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quitting_releases_the_realm_lease() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let rig = Attached::start(&ws.root, &llm).await;
    let realm = common::realm_of(&ws.root);
    assert_eq!(walk_leases(&realm), 1);
    rig.quit().await;
    assert_eq!(walk_leases(&realm), 0);
}

/// Lease records (`<realm>/leases/*.json`) under `dir`.
fn walk_leases(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                walk_leases(&path)
            } else {
                usize::from(
                    path.extension().is_some_and(|ext| ext == "json")
                        && path
                            .parent()
                            .and_then(|p| p.file_name())
                            .is_some_and(|n| n == "leases"),
                )
            }
        })
        .sum()
}

/// `!echo hi` through the runner: the command runs in the workspace (here
/// unconfined, like the agent's shell under `--no-confinement`), its output
/// comes back, and the next model call reads it as system context without a
/// turn having been started for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bang_command_runs_and_joins_the_context_without_a_turn() {
    use rkat_tui::ui::{ShellOutcome, ShellRunId};
    use rkat_tui::user_shell::ShellExit;

    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("seen")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::RunShell {
        id: ShellRunId(1),
        command: "echo hi; pwd".into(),
    });
    let msg = rig
        .wait_for(|msg| matches!(msg, Msg::ShellFinished { .. }))
        .await;
    let Msg::ShellFinished {
        id,
        outcome: ShellOutcome::Ran { run, context },
    } = msg
    else {
        panic!("{msg:?}");
    };
    assert_eq!(id, ShellRunId(1));
    assert_eq!(run.exit, ShellExit::Code(0));
    let root = std::fs::canonicalize(&ws.root).unwrap();
    assert!(run.output.contains("hi\n"), "{run:?}");
    assert!(run.output.contains(&root.display().to_string()), "{run:?}");
    assert_eq!(context, Ok(()));
    // No model call yet: the command started no turn.
    assert!(llm.recorded().is_empty());

    rig.send(Effect::StartTurn("what did it say?".into()));
    let _ = rig.turn_finished().await;
    let system = llm.recorded()[0].system();
    assert!(
        system.contains("[shell run by the user] $ echo hi; pwd\nhi\n"),
        "{system}"
    );
    assert!(system.contains("(exit 0)"), "{system}");
    assert_eq!(
        user_texts(&llm.requests()[0]),
        vec!["what did it say?".to_string()]
    );
    rig.quit().await;
}

/// Plan mode end to end: the session is told, a write the model tries is
/// refused with the plan-mode error the model reads (nothing is written,
/// nobody is asked), the mode is recorded in the realm's `tui.toml`, and
/// leaving plan mode tells the session so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_mode_refuses_a_write_and_the_model_reads_why() {
    use rkat_tui::approval::mode::PermissionMode;

    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "w",
            name: "write_file",
            args: json!({ "path": "src/new.rs", "content": "pub fn f() {}\n" }),
        },
        Step::Text("1. Add src/new.rs with f()"),
        Step::Text("ok"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let mode_is = |want: PermissionMode| move |msg: &Msg| matches!(msg, Msg::Status(StatusUpdate::Mode { mode, .. }) if *mode == want);
    rig.send(Effect::SetMode(PermissionMode::Plan));
    rig.wait_for(mode_is(PermissionMode::Plan)).await;
    rig.send(Effect::StartTurn("plan a new module".into()));
    assert_eq!(
        rig.turn_finished().await.expect("turn").text,
        "1. Add src/new.rs with f()"
    );
    assert!(!ws.root.join("src/new.rs").exists());
    assert!(
        !rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::Escalation(..))),
        "nobody is asked"
    );
    let result = llm
        .requests()
        .iter()
        .flatten()
        .find_map(|m| match m {
            Message::ToolResults { results, .. } => {
                results.iter().find(|r| r.tool_use_id == "w").cloned()
            }
            _ => None,
        })
        .expect("a result for the write");
    assert!(result.is_error, "{result:?}");
    assert!(
        result.text_content().contains("plan mode is on"),
        "{}",
        result.text_content()
    );
    let system = llm.recorded()[0].system();
    assert!(system.contains("# Plan mode is on"), "{system}");
    let realm_rules = std::fs::read_to_string(common::realm_of(&ws.root).join("tui.toml")).unwrap();
    assert!(realm_rules.contains("mode = \"plan\""), "{realm_rules}");

    rig.send(Effect::SetMode(PermissionMode::AcceptEdits));
    rig.wait_for(mode_is(PermissionMode::AcceptEdits)).await;
    rig.send(Effect::StartTurn("go".into()));
    let _ = rig.turn_finished().await;
    let system = llm.recorded().last().unwrap().system();
    assert!(system.contains("# Plan mode is off"), "{system}");
    let realm_rules = std::fs::read_to_string(common::realm_of(&ws.root).join("tui.toml")).unwrap();
    assert!(
        realm_rules.contains("mode = \"accept-edits\""),
        "{realm_rules}"
    );
    rig.quit().await;
}

/// The model's task tools fill the Ctrl+T checklist from the engine's task
/// store, and a resumed session gets its checklist back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_task_tools_fill_the_checklist_and_resume_restores_it() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use rkat_tui::ui::{App, ChecklistView, ItemState, StatusInfo};

    let ws = Workspace::new();
    let create = |subject: &str| json!({ "subject": subject, "description": "" });
    let llm = ScriptedLlmClient::new(vec![
        Step::Tools(vec![
            ("c1", "task_create", create("read the parser")),
            ("c2", "task_create", create("write the fix")),
            ("c3", "task_create", create("run the tests")),
        ]),
        Step::Dynamic(Arc::new(|messages| {
            Step::Tools(vec![
                (
                    "u1",
                    "task_update",
                    json!({ "id": task_id(messages, "read the parser"), "status": "completed" }),
                ),
                (
                    "u2",
                    "task_update",
                    json!({ "id": task_id(messages, "write the fix"), "status": "in_progress" }),
                ),
            ])
        })),
        Step::Text("on it"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let first = rig.current_session().await;
    let mut app = App::new(StatusInfo::default());
    rig.send(Effect::StartTurn("fix the parser".into()));
    let done_and_in_progress = |msg: &Msg| {
        matches!(msg, Msg::Checklist(list) if list.items().iter().any(|i| i.state == ItemState::InProgress)
            && list.items().iter().any(|i| i.state == ItemState::Done))
    };
    pump(&mut rig, &mut app, done_and_in_progress).await;
    let _ = rig.turn_finished().await;
    let ctrl_t = Msg::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    let fx = app.update(ctrl_t.clone());
    assert!(fx.contains(&Effect::SetChecklistView(ChecklistView::Shown)));
    for effect in fx {
        rig.send(effect);
    }
    let want = [
        ("run the tests", ItemState::Pending),
        ("read the parser", ItemState::Done),
        ("write the fix", ItemState::InProgress),
    ];
    // The two updates ran as parallel tool calls, so their order among
    // themselves is not fixed: the pending item first, the others as a set.
    let items = |list: &rkat_tui::ui::Checklist| -> Vec<(String, ItemState)> {
        let mut items: Vec<(String, ItemState)> = list
            .items()
            .iter()
            .map(|i| (i.text.clone(), i.state))
            .collect();
        if items.len() > 1 {
            items[1..].sort_by(|a, b| a.0.cmp(&b.0));
        }
        items
    };
    let want: Vec<(String, ItemState)> = want.iter().map(|(t, s)| (t.to_string(), *s)).collect();
    assert_eq!(items(app.checklist_panel().expect("shown")), want);
    // The creates showed up before the updates (a read after each
    // completed task call).
    assert!(
        rig.seen.iter().any(|m| matches!(m, Msg::Checklist(list)
            if list.items().len() == 3 && list.items().iter().all(|i| i.state == ItemState::Pending))),
        "{:?}",
        rig.seen
    );
    rig.quit().await;
    let realm_rules = std::fs::read_to_string(common::realm_of(&ws.root).join("tui.toml")).unwrap();
    assert!(
        realm_rules.contains("checklist = \"shown\""),
        "{realm_rules}"
    );

    // Restored by a restarted daemon: the same list, read from the store,
    // before any turn.
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&ws.root, &llm, client_args(&["-c"])).await;
    assert_eq!(rig.current_session().await, first);
    rig.saw(|msg| matches!(msg, Msg::Checklist(list) if !list.is_empty()))
        .await;
    let list = rig
        .seen
        .iter()
        .find_map(|msg| match msg {
            Msg::Checklist(list) if !list.is_empty() => Some(list.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(items(&list), want);

    // A new conversation starts with an empty checklist.
    rig.send(Effect::NewSession);
    rig.wait_for(|msg| matches!(msg, Msg::ResetTranscript(blocks) if blocks.is_empty()))
        .await;
    let msg = rig.wait_for(|msg| matches!(msg, Msg::Checklist(_))).await;
    assert!(matches!(msg, Msg::Checklist(list) if list.is_empty()));
    rig.quit().await;
}

/// Every path under `root`, for the rename test's diagnostics.
fn remaining_entries(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            out.push(path.display().to_string());
            if path.is_dir() {
                out.extend(remaining_entries(&path));
            }
        }
    }
    out
}

/// A steer the runtime abandons before any run takes it (another client
/// of the agent force-cancels its work, which abandons the input it held
/// queued) is not left "delivering": once the daemon says the agent is
/// idle with nothing held, the client asks what became of it
/// (`rkat/inputs`), and it returns to the editor with a notice saying why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_the_runtime_abandoned_returns_to_the_editor() {
    let ws = Workspace::new();
    let gate = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(
            Arc::clone(&gate),
            Box::new(Step::Text("cancelled before this")),
        ),
        Step::Text("never asked"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let agent = rig.agent().await;
    rig.send(Effect::StartTurn("hang".into()));
    llm.wait_call(0).await;
    rig.send(steer(1, "typed while it hangs"));
    let ack = rig
        .wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    assert!(
        matches!(ack, Msg::SteerAck { result: Ok(()), .. }),
        "{ack:?}"
    );
    let other = rig.daemon.connect_bound().await;
    other
        .call(
            "mob/force_cancel",
            json!({"mob_id": "tui", "agent_identity": agent}),
        )
        .await
        .unwrap();
    // The cancel takes the run at its next boundary.
    gate.notify_one();
    let fate = rig
        .wait_for(|msg| {
            matches!(
                msg,
                Msg::SteerDelivered(PendingId(1)) | Msg::NotDelivered { .. }
            )
        })
        .await;
    assert!(
        matches!(&fate, Msg::NotDelivered { id: PendingId(1), why: Some(why) } if why == "it was cancelled"),
        "{fate:?}"
    );
    rig.saw(|msg| matches!(msg, Msg::TurnFinished(_))).await;
    assert_eq!(llm.requests().len(), 1, "no run took it");
    let session = rig.messages().await;
    assert!(!user_texts(&session).contains(&"typed while it hangs".to_string()));
    rig.quit().await;
}

/// The owner's rule: Esc takes back a steer the agent has
/// not read, so it starts no follow-up run; it is returned (to the editor),
/// never delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_queued_behind_an_interrupted_run_comes_back() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Hang, Step::Text("after")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let agent = rig.agent().await;
    rig.send(Effect::StartTurn("hang".into()));
    rig.wait_for(|msg| matches!(msg, Msg::Agent(env) if matches!(env.payload, AgentEvent::RunStarted { .. })))
        .await;
    llm.wait_call(0).await;
    rig.send(steer(1, "typed while it hangs"));
    rig.wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    rig.send(Effect::Interrupt);
    assert_eq!(rig.turn_finished().await, Err(TurnFailure::Interrupted));
    assert!(
        rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::Returned(ids) if ids == &[PendingId(1)])),
        "{:#?}",
        rig.seen
    );
    assert!(
        !rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))))
    );
    rig.daemon.idle(&agent).await;
    assert_eq!(llm.requests().len(), 1, "no follow-up run");
    let session = rig.messages().await;
    assert!(!user_texts(&session).contains(&"typed while it hangs".to_string()));
    rig.quit().await;
}

/// How many of the conversation's user messages are `text`.
fn count(messages: &[Message], text: &str) -> usize {
    user_texts(messages).iter().filter(|t| *t == text).count()
}

/// The sequence of a user report (a model request that sends nothing): a
/// steer typed during the stall, then an interrupt and a message sent once
/// the turn ends. The interrupt is confirmed well within the client's
/// deadline, the stalled run ends, the steer comes back (the owner's
/// rule), and sent again with the new message it reaches the
/// conversation once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupt_during_a_stalled_model_request_ends_it_and_loses_nothing() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Hang,
        Step::Text("for the steer"),
        Step::Text("for the message"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("stall".into()));
    rig.wait_for(|msg| matches!(msg, Msg::Agent(env) if matches!(env.payload, AgentEvent::RunStarted { .. })))
        .await;
    llm.wait_call(0).await;
    rig.send(steer(1, "steered during the stall"));
    rig.wait_for(|msg| matches!(msg, Msg::SteerAck { result: Ok(()), .. }))
        .await;
    // Admitted, not written into the stalled request.
    assert!(
        !rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))))
    );
    let asked = std::time::Instant::now();
    rig.send(Effect::Interrupt);
    assert_eq!(rig.turn_finished().await, Err(TurnFailure::Interrupted));
    let took = asked.elapsed();
    assert!(
        took < rkat_tui::client::runner::INTERRUPT_DEADLINE / 2,
        "{took:?}"
    );
    assert!(
        !rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("did not confirm"))),
        "{:#?}",
        rig.seen
    );
    // The steer came back (the owner's rule); the new message goes
    // once the turn ended, with it as the user sends it again.
    assert!(
        rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::Returned(ids) if ids == &[PendingId(1)])),
        "{:#?}",
        rig.seen
    );
    rig.send(Effect::StartTurn(
        "steered during the stall\n\nsent after the interrupt".into(),
    ));
    let done = rig.turn_finished().await.unwrap();
    assert_eq!(done.text, "for the steer");
    let session = rig.messages().await;
    assert_eq!(llm.requests().len(), 2, "{session:#?}");
    assert!(
        user_texts(&session)
            .iter()
            .any(|text| text.contains("steered during the stall")),
        "{session:#?}"
    );
    // The engine closes an interrupted run's tail without its prompt: the
    // conversation (and so the model) no longer has it.
    assert_eq!(count(&session, "stall"), 0, "{session:#?}");
    rig.quit().await;
}

/// The same with a fork (`assign` with `context: "fork"`) running in the background (its own
/// model request stalled too): the agent's stalled request is still
/// interrupted at once, and the next message is answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupt_during_a_stalled_request_is_confirmed_while_a_fork_runs() {
    const TASK: &str = "FORK-CHILD-TASK stall too";
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let text = format!("{messages:?}");
        // The child forked before the message after the interrupt.
        if text.contains("sent after the interrupt") {
            Step::Text("answered")
        } else if text.contains("FORK-CHILD-TASK") || text.contains("fo1") {
            Step::Hang
        } else {
            common::fork("fo1", "forky", TASK, false)
        }
    }));
    let llm = ScriptedLlmClient::new(vec![step; 8]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("fork, then stall".into()));
    // The parent's stalled request and the child's.
    llm.wait_call(2).await;
    let asked = std::time::Instant::now();
    rig.send(Effect::Interrupt);
    assert_eq!(rig.turn_finished().await, Err(TurnFailure::Interrupted));
    let took = asked.elapsed();
    assert!(
        took < rkat_tui::client::runner::INTERRUPT_DEADLINE / 2,
        "{took:?}"
    );
    rig.send(Effect::StartTurn("sent after the interrupt".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "answered");
    rig.quit().await;
}

/// The order of the user report: a model request held open, a steer sent
/// ("delivering…"), the interrupt, the steer taken back (↑),
/// and the message sent again. The interrupt is confirmed at once, the
/// recalled steer is never run, and the message sent again reaches the
/// conversation exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_then_recall_then_resend_during_a_stall_delivers_once() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Hang, Step::Text("answered")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::StartTurn("stall".into()));
    rig.wait_for(|msg| matches!(msg, Msg::Agent(env) if matches!(env.payload, AgentEvent::RunStarted { .. })))
        .await;
    llm.wait_call(0).await;
    rig.send(steer(1, "the message"));
    rig.wait_for(|msg| matches!(msg, Msg::SteerAck { result: Ok(()), .. }))
        .await;
    let asked = std::time::Instant::now();
    rig.send(Effect::Interrupt);
    rig.send(Effect::RecallQueued);
    let recalled = rig
        .wait_for(|msg| matches!(msg, Msg::Recalled { .. }))
        .await;
    assert_eq!(rig.turn_finished().await, Err(TurnFailure::Interrupted));
    let took = asked.elapsed();
    assert!(
        took < rkat_tui::client::runner::INTERRUPT_DEADLINE / 2,
        "{took:?}"
    );
    // Taken back by the recall, or by the interrupt itself (it returns
    // what the agent has not read).
    let taken_back = matches!(&recalled, Msg::Recalled { recalled, .. } if recalled.contains(&PendingId(1)))
        || rig
            .seen
            .iter()
            .any(|msg| matches!(msg, Msg::Returned(ids) if ids.contains(&PendingId(1))));
    let delivered = rig
        .seen
        .iter()
        .any(|msg| matches!(msg, Msg::SteerDelivered(PendingId(1))));
    // Either the recall took it back or it ran: never both, never neither.
    assert!(taken_back != delivered, "{recalled:?} {:#?}", rig.seen);
    if taken_back {
        rig.send(Effect::StartTurn("the message".into()));
        assert_eq!(rig.turn_finished().await.unwrap().text, "answered");
    } else {
        rig.saw(|msg| matches!(msg, Msg::TurnFinished(_))).await;
    }
    let session = rig.messages().await;
    assert_eq!(count(&session, "the message"), 1, "{session:#?}");
    assert!(
        !rig.seen
            .iter()
            .any(|msg| matches!(msg, Msg::Notice { text, .. } if text.contains("did not confirm"))),
        "{:#?}",
        rig.seen
    );
    rig.quit().await;
}

/// The client's fallback when a stop stops nothing or goes unconfirmed:
/// `mob/hard_cancel_member`, forwarded by the daemon, ends the agent's
/// stalled run at once, and the agent serves the next prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hard_cancel_through_the_daemon_ends_a_stalled_run() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Hang, Step::Text("after")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let agent = rig.agent().await;
    rig.send(Effect::StartTurn("stall".into()));
    llm.wait_call(0).await;
    let other = rig.daemon.connect_bound().await;
    let answer = other
        .call(
            "mob/hard_cancel_member",
            json!({"mob_id": "tui", "agent_identity": agent, "reason": "test"}),
        )
        .await
        .unwrap();
    assert_eq!(answer["cancelled"], true, "{answer}");
    assert_eq!(rig.turn_finished().await, Err(TurnFailure::Interrupted));
    rig.send(Effect::StartTurn("again".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "after");
    rig.quit().await;
}

/// Starts a run of the daemon's first agent from another client (a raw
/// connection bound to it, which the caller keeps open as that client),
/// held open by the scripted model: a run the client under test did not
/// start.
async fn start_other_run(daemon: &rig::Running) -> rkat_tui::client::transport::Connection {
    let connection = daemon.connect_bound().await;
    connection
        .call(
            "mob/member_send",
            json!({"mob_id": "tui", "agent_identity": daemon.agent, "content": "another client's run"}),
        )
        .await
        .unwrap();
    connection
}

/// A client that attached while its agent ran (so it has
/// no turn of its own for that run) interrupts it with Esc, and says what
/// it did; before, the press did nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn esc_interrupts_a_run_the_client_did_not_start() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Hang, Step::Text("sentinel answered")]);
    let daemon = rig::Running::start(&ws.root, &llm).await;
    let other = start_other_run(&daemon).await;
    llm.wait_call(0).await;
    let mut client = rig::Client::attach_agent(
        &ws.root,
        rkat_tui::daemon::protocol::AgentChoice::Agent {
            name: daemon.agent.clone(),
        },
    )
    .await;
    client.send(Effect::Interrupt);
    // Either way the client follows it (it joined the run from its replay,
    // or it had no turn and asked the daemon), the press is answered.
    let answer = client
        .wait_for(|msg| {
            matches!(msg, Msg::TurnFinished(Err(TurnFailure::Interrupted)))
                || matches!(msg, Msg::Notice { text, .. } if text.contains("interrupted the agent's run"))
        })
        .await;
    assert!(!matches!(answer, Msg::Notice { ref text, .. } if text.contains("nothing is running")));
    // The other client leaves once its run is over.
    drop(other);
    daemon.idle(&daemon.agent).await;
    client.send(Effect::StartTurn("sentinel".into()));
    // (The interrupted run may end this client's view of it first.)
    client
        .wait_for(
            |msg| matches!(msg, Msg::TurnFinished(Ok(done)) if done.text == "sentinel answered"),
        )
        .await;
    client.quit().await;
    daemon.stop().await;
}

/// Esc with nothing running says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn esc_with_nothing_running_says_so() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    rig.send(Effect::Interrupt);
    rig.wait_for(|msg| matches!(msg, Msg::Notice { text, .. } if text == "nothing is running"))
        .await;
    assert!(llm.requests().is_empty());
    rig.quit().await;
}

/// Ctrl-X Ctrl-S with text while a run another client
/// started is open (the UI shows it as a remote turn) interrupts that run,
/// then sends the text as the next turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_now_interrupts_another_clients_run_then_sends() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Hang, Step::Text("sent after it")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let other = start_other_run(&rig.daemon).await;
    llm.wait_call(0).await;
    rig.wait_for(|msg| matches!(msg, Msg::RemoteTurn { .. }))
        .await;
    // Ctrl-X Ctrl-S with text while running: the interrupt now, the text once
    // the turn ended (the UI's `send_next`).
    rig.send(Effect::Interrupt);
    assert_eq!(rig.turn_finished().await, Err(TurnFailure::Interrupted));
    // The other client leaves once its run is over.
    drop(other);
    rig.send(Effect::StartTurn("typed meanwhile".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "sent after it");
    assert!(user_texts(&llm.requests()[1]).contains(&"typed meanwhile".to_string()));
    rig.quit().await;
}

/// Feed `app` every message of `rig.seen` from `fed` on, forwarding the
/// effects it answers with; returns the new count fed.
fn feed(rig: &Attached, app: &mut rkat_tui::ui::App, fed: usize) -> usize {
    let msgs: Vec<Msg> = rig.seen[fed..].to_vec();
    for msg in msgs {
        for effect in app.update(msg) {
            rig.send(effect);
        }
    }
    rig.seen.len()
}

/// The labels of `app`'s pending placeholders, in order.
fn pending_labels(app: &rkat_tui::ui::App) -> Vec<rkat_tui::ui::transcript::QueuedLabel> {
    app.transcript
        .blocks()
        .iter()
        .filter_map(|block| match block {
            Block::Queued { label, .. } => Some(*label),
            _ => None,
        })
        .collect()
}

/// The owner's report: runs no client started came one after another (a
/// helper's message each, here the daemon's notes, which start runs the
/// same way), and the user's Enter landed between two of them: the UI
/// still showed the first run, the runner had ended its turn, so the
/// steer was refused (`NotRunning`) and went again as a prompt. That
/// prompt was a `queue` input, and the next run had begun meanwhile: it
/// waited behind that whole run (measured before: request 2, the next
/// one after it, did not carry it; it ran as a fourth request of its
/// own). Every input of the user's is a steer now: it rides the next
/// model request of the run that began, and the UI shows it sent, never
/// held for a turn's end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_typed_as_a_run_ends_rides_the_next_run_s_next_request() {
    use common::script::{Script, call, hold, make};
    let ws = Workspace::new();
    let g1 = Arc::new(Notify::new());
    let g2 = Arc::new(Notify::new());
    let llm = Script::new()
        .after("c2", make(|_| Step::Text("r2 done")))
        .input("TYPED", Step::Text("typed answered"))
        .input("NOTE-2", hold(&g2, call("c2", "contacts", json!({}))))
        .input("NOTE-1", hold(&g1, Step::Text("r1 done")))
        .otherwise(Step::Text("other"))
        .client(12);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let mut app = rkat_tui::ui::App::new(rkat_tui::ui::StatusInfo::default());
    let mut fed = feed(&rig, &mut app, 0);
    let agent = rig.agent().await;
    let local = rig.daemon.local();
    let note = |text: &'static str| {
        let (local, agent) = (Arc::clone(&local), agent.clone());
        tokio::spawn(async move { local.steer_for_test(&agent, text).await })
    };
    let n1 = note("NOTE-1");
    llm.wait_call(0).await;
    rig.wait_for(|msg| matches!(msg, Msg::RemoteTurn { .. }))
        .await;
    fed = feed(&rig, &mut app, fed);
    assert!(app.is_running());
    g1.notify_one();
    n1.await.unwrap().unwrap();
    rig.wait_for(|msg| matches!(msg, Msg::TurnFinished(_)))
        .await;
    // The UI has not drawn the turn's end yet: Enter steers, and the
    // runner, its turn over, refuses the steer.
    type_into(&rig, &mut app, "TYPED");
    let ack = rig
        .wait_for(|msg| matches!(msg, Msg::SteerAck { .. }))
        .await;
    assert!(
        matches!(
            ack,
            Msg::SteerAck {
                result: Err(rkat_tui::ui::delivery::SteerFailure::NotRunning),
                ..
            }
        ),
        "{ack:?}"
    );
    // The next run begins (its first model request held) before the UI
    // reads the turn's end and the refusal.
    let n2 = note("NOTE-2");
    llm.wait_call(1).await;
    rig.wait_for(|msg| matches!(msg, Msg::RemoteTurn { .. }))
        .await;
    let sends = rig.daemon.forwarded("mob/member_send");
    feed(&rig, &mut app, fed);
    assert!(
        pending_labels(&app).is_empty(),
        "sent, not held: {:?}",
        pending_labels(&app)
    );
    assert!(
        app.transcript
            .blocks()
            .iter()
            .any(|block| matches!(block, Block::User { text } if text == "TYPED"))
    );
    assert!(app.is_running());
    common::until(|| rig.daemon.forwarded("mob/member_send") > sends).await;
    g2.notify_one();
    n2.await.unwrap().unwrap();
    // The run's next request carries it.
    llm.wait_call(2).await;
    assert!(
        common::run_inputs(&llm.requests()[2]).contains("TYPED"),
        "{:?}",
        llm.requests()[2]
    );
    let finished = pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::TurnFinished(_))
    })
    .await;
    assert!(
        matches!(&finished, Msg::TurnFinished(Ok(summary)) if summary.text == "r2 done"),
        "{finished:?}"
    );
    assert_eq!(llm.requests().len(), 3, "no run of its own after it");
    assert!(!app.is_running());
    rig.quit().await;
}

/// Type `text` into `app` and press Enter, forwarding its effects.
fn type_into(rig: &Attached, app: &mut rkat_tui::ui::App, text: &str) {
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    for c in text.chars() {
        app.update(Msg::Key(KeyEvent::from(KeyCode::Char(c))));
    }
    for effect in app.update(Msg::Key(KeyEvent::from(KeyCode::Enter))) {
        rig.send(effect);
    }
}

/// Commands typed during a turn that need the agent idle wait for its
/// end, in one order with the messages typed after them: `/model` typed
/// during a held turn switches once it ends, and the message typed after
/// it runs on the new model; a held command taken back (↑) never runs;
/// `/compact` during the turn does not wait, the engine takes the request
/// at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commands_typed_during_a_turn_run_when_it_ends_in_order() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    let ws = Workspace::new();
    let gate = Arc::new(Notify::new());
    let llm = ScriptedLlmClient::new(vec![
        Step::Gated(Arc::clone(&gate), Box::new(Step::Text("first done"))),
        // The compaction `/compact` asked for, at the next run's check.
        Step::Text("SUMMARY of the earlier turns"),
        Step::Text("after the switch"),
        Step::Text("spare"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    let mut app = rkat_tui::ui::App::new(rkat_tui::ui::StatusInfo::default());
    for msg in rig.seen.clone() {
        for effect in app.update(msg) {
            rig.send(effect);
        }
    }
    type_into(&rig, &mut app, "work");
    llm.wait_call(0).await;
    // Held: a model switch, an /init taken back, a message after them.
    type_into(&rig, &mut app, "/model anthropic:claude-opus-4-8");
    type_into(&rig, &mut app, "/init");
    assert!(app.update(Msg::Key(KeyEvent::from(KeyCode::Up))).is_empty());
    assert_eq!(app.input.text(), "/init");
    app.input.clear();
    type_into(&rig, &mut app, "on the new model");
    // `/compact` goes now.
    app.status.apply(StatusUpdate::Context {
        used: Some(1_000),
        approx: false,
    });
    type_into(&rig, &mut app, "/compact");
    let answered = pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::CompactRequested(_))
    })
    .await;
    assert!(
        matches!(answered, Msg::CompactRequested(Ok(()))),
        "{answered:?}"
    );
    assert_eq!(llm.recorded().len(), 1, "nothing ran before the turn's end");
    gate.notify_one();
    pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::Notice { text, .. } if text.starts_with("switched to claude-opus-4-8"))
    })
    .await;
    let finished = pump(&mut rig, &mut app, |msg| {
        matches!(msg, Msg::TurnFinished(_))
    })
    .await;
    assert!(
        matches!(&finished, Msg::TurnFinished(Ok(summary)) if summary.text == "after the switch"),
        "{finished:?}"
    );
    let recorded = llm.recorded();
    let models: Vec<&str> = recorded.iter().map(|r| r.model.as_str()).collect();
    assert_eq!(models[0], "claude-sonnet-4-6");
    assert!(
        models[1..].iter().all(|model| *model == "claude-opus-4-8"),
        "{models:?}"
    );
    assert!(recorded.last().unwrap().user().contains("on the new model"));
    // The compaction asked for mid-turn was consumed by the next run's
    // check.
    assert!(rig.events().iter().any(|e| matches!(
        e,
        AgentEvent::CompactionStarted { .. } | AgentEvent::RequestedCompactionSkipped { .. }
    )));
    assert!(
        !recorded
            .iter()
            .any(|r| r.user().contains(rkat_tui::ui::commands::INIT_PROMPT)),
        "the taken-back /init never ran"
    );
    rig.quit().await;
}
