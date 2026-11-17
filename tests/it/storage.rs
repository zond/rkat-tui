//! A storage failure on the rig: the agent left as a failed durable commit
//! leaves it (the engine's own test hook; the rig cannot fill a disk), so
//! the engine refuses its next send with `MemberReloadRequired`. The
//! daemon reloads it; no daemon restart.
//!
//! Each case starts from a fresh agent with no run behind it: the hook
//! applied while a run's terminal is still being written fails that write
//! too, and the engine's reload then refuses the ambiguous terminal (a
//! state the hook makes, `docs/evidence.md`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, user_texts};
use rig::Attached;
use rkat_tui::daemon::storage::RELOADED_LINE;
use rkat_tui::ui::{Effect, Msg, TurnFailure};

/// The engine's own words, never shown.
const RAW: &[&str] = &[
    "requires a runtime reload",
    "-32029",
    "durability",
    "mob/member_send failed",
];

fn texts(seen: &[Msg]) -> Vec<String> {
    seen.iter()
        .filter_map(|msg| match msg {
            Msg::Notice { text, .. } => Some(text.clone()),
            Msg::TurnFinished(Err(TurnFailure::Failed(text))) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn no_engine_words(seen: &[Msg]) {
    for text in texts(seen) {
        for raw in RAW {
            assert!(!text.contains(raw), "{raw}: {text}");
        }
    }
}

/// A fresh agent the engine needs reloaded, and its rig.
async fn degraded(ws: &Workspace, llm: &Arc<ScriptedLlmClient>) -> (Attached, String) {
    let mut rig = Attached::start(&ws.root, llm).await;
    let agent = rig.agent().await;
    assert!(rig.daemon.local().degrade_for_test(&agent).await);
    (rig, agent)
}

/// The user's message to an agent the engine needs reloaded: the daemon
/// reloads it, the client opens the agent's stream again and sends the
/// message once more, so the model is called for it (positive proof: its
/// request carries the message) and the turn answers; the user sees one
/// plain line saying the agent was reloaded and that the step in flight
/// was not saved. The next message needs nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_next_message_reloads_the_agent_and_is_answered() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Text("answer after the reload"),
        Step::Text("the next answer"),
    ]);
    let (mut rig, agent) = degraded(&ws, &llm).await;
    rig.send(Effect::StartTurn("one after the failure".into()));
    assert_eq!(
        rig.turn_finished().await.unwrap().text,
        "answer after the reload"
    );
    assert_eq!(llm.requests().len(), 1);
    assert!(
        user_texts(&llm.requests()[0]).contains(&"one after the failure".to_string()),
        "{:?}",
        user_texts(&llm.requests()[0])
    );
    let shown = texts(&rig.seen);
    assert_eq!(
        shown.iter().filter(|t| *t == RELOADED_LINE).count(),
        1,
        "{shown:#?}"
    );
    rig.daemon.idle(&agent).await;
    rig.send(Effect::StartTurn("two".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "the next answer");
    no_engine_words(&rig.seen);
    rig.quit().await;
}

/// A reload that fails (the disk still full) is said plainly: free space
/// on the realm's filesystem, then send again; the model is not called.
/// The next message, with the reload working, goes through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_reload_is_said_plainly_and_the_next_message_retries() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("answer once the disk has room")]);
    let (mut rig, _agent) = degraded(&ws, &llm).await;
    rig.daemon.local().fail_reloads_for_test(1);
    rig.send(Effect::StartTurn("one while full".into()));
    let Err(TurnFailure::Failed(why)) = rig.turn_finished().await else {
        panic!("the turn fails");
    };
    let realm = rig.daemon.local().core().realm.display().to_string();
    assert!(
        why.contains("could not be reloaded") && why.contains("free space") && why.contains(&realm),
        "{why}"
    );
    assert!(llm.requests().is_empty(), "the model is not called");
    rig.send(Effect::StartTurn("two with room".into()));
    assert_eq!(
        rig.turn_finished().await.unwrap().text,
        "answer once the disk has room"
    );
    assert_eq!(llm.requests().len(), 1);
    no_engine_words(&rig.seen);
    rig.quit().await;
}

/// A note of the daemon's own to an agent the engine needs reloaded: the
/// daemon reloads it and sends the note again, and the agent's run reads
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_note_reloads_the_agent_and_reaches_it() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("read the note")]);
    let (rig, agent) = degraded(&ws, &llm).await;
    rig.daemon
        .local()
        .steer_for_test(&agent, "a note from the daemon")
        .await
        .unwrap();
    common::until(|| llm.requests().len() == 1).await;
    assert!(user_texts(&llm.requests()[0]).contains(&"a note from the daemon".to_string()));
    no_engine_words(&rig.seen);
    rig.quit().await;
}
