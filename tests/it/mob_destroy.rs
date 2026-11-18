//! `mob_destroy` of a mob the agent created (P6 item 18): the destroy
//! succeeds and the mob's database is gone from disk, in the daemon that
//! made it and in a later one that restored it. A mob a client follows
//! (a council's) is `tests/councils.rs`'s: the daemon closes the client's
//! stream on it when it is destroyed (`docs/evidence.md`, "`mob_destroy`").

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::path::PathBuf;
use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::Attached;
use rkat_tui::ui::Effect;
use serde_json::json;

/// The prompt that makes the team (`make`) or destroys it (`destroy`).
fn script() -> Arc<ScriptedLlmClient> {
    // Each step from what the request answers: the member's idle note can
    // share a request with the user's prompt or a tool result, before or
    // after it.
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if common::is_helper(messages, "hello") {
            // The member's own turns.
            return Step::Text("ready");
        }
        let inputs = common::run_inputs(messages);
        if common::answers(messages, Some("d1")) {
            return Step::Text("gone");
        }
        if inputs.contains("destroy") {
            return Step::Tool {
                id: "d1",
                name: "mob_destroy",
                args: json!({"mob_id": "team"}),
            };
        }
        if common::answers(messages, Some("s1")) {
            return Step::Text("made");
        }
        if common::answers(messages, Some("m1")) {
            return Step::Tool {
                id: "s1",
                name: "mob_spawn_member",
                args: json!({"mob_id": "team", "profile": "worker", "member_id": "w1", "initial_message": "hello"}),
            };
        }
        if inputs.contains("make") {
            return Step::Tool {
                id: "m1",
                name: "mob_create",
                args: json!({"definition": {"id": "team", "profiles": {"worker": {"model": common::MODEL, "tools": {"comms": true}}}}}),
            };
        }
        // The note that the member went idle (it stays seated): taken in,
        // nothing done.
        Step::Text("noted")
    }));
    ScriptedLlmClient::new(vec![step; 40])
}

/// Reveal the mob tools the script calls (`tests/deferral.rs` covers
/// finding them).
async fn reveal(rig: &Attached) {
    let session = rig.current_session().await;
    rig.daemon
        .reveal(&session, &["mob_create", "mob_spawn_member", "mob_destroy"])
        .await;
}

/// The member's first turn ended and the agent read the note that it went
/// idle, and is idle again: nothing of the make is left to land in the
/// destroy's turn.
async fn made_and_told(rig: &Attached, llm: &ScriptedLlmClient) {
    common::until(|| {
        llm.recorded()
            .iter()
            .any(|r| !r.user().ends_with("hello") && r.user().contains("went idle"))
    })
    .await;
    rig.daemon.idle(&rig.daemon.agent).await;
}

/// The mob `team`'s database.
fn database(ws: &Workspace) -> PathBuf {
    rkat_tui::bootstrap::resolve_scope(&common::args_with(&ws.root, &[]))
        .unwrap()
        .realm_root()
        .join("mobs")
        .join("mob--team.db")
}

/// The result of the agent's `mob_destroy` in `llm`'s last request.
fn destroyed(llm: &ScriptedLlmClient) -> serde_json::Value {
    let last = llm.recorded().pop().unwrap();
    let text = last
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::ToolResults { results, .. } => results
                .iter()
                .find(|r| r.tool_use_id == "d1")
                .map(|r| r.text_content()),
            _ => None,
        })
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agent_destroys_a_mob_it_made_and_its_database_goes() {
    let ws = Workspace::new();
    let llm = script();
    let mut rig = Attached::start(&ws.root, &llm).await;
    reveal(&rig).await;
    rig.send(Effect::StartTurn("make".into()));
    // Its text is `noted` when the member's idle note reached the run.
    assert!(rig.turn_finished().await.is_ok());
    assert!(database(&ws).exists());
    made_and_told(&rig, &llm).await;
    rig.send(Effect::StartTurn("destroy".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "gone");
    let answer = destroyed(&llm);
    assert_eq!(answer["ok"], true, "{answer}");
    assert!(!database(&ws).exists());
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mob_restored_by_a_later_daemon_is_destroyed_with_its_database() {
    let ws = Workspace::new();
    let llm = script();
    let mut rig = Attached::start(&ws.root, &llm).await;
    reveal(&rig).await;
    rig.send(Effect::StartTurn("make".into()));
    assert!(rig.turn_finished().await.is_ok());
    // The member's first turn ended before the daemon stops.
    made_and_told(&rig, &llm).await;
    let first = Arc::downgrade(&rig.daemon.state());
    rig.quit().await;
    assert!(database(&ws).exists());
    // Nothing of the first daemon outlives its stop: its mob host held
    // the mob's handle, which kept the database observed, and the second
    // daemon's destroy could not drain it (an idle question left asking
    // kept it for up to ten seconds, `Activity::stop`).
    assert!(
        first.upgrade().is_none(),
        "the first daemon's mob host outlived its stop"
    );

    let llm = script();
    let mut rig = Attached::start(&ws.root, &llm).await;
    reveal(&rig).await;
    rig.send(Effect::StartTurn("destroy".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "gone");
    let answer = destroyed(&llm);
    assert_eq!(answer["ok"], true, "{answer}");
    assert!(!database(&ws).exists());
    rig.quit().await;
}
