//! `mob_destroy` of a mob the agent created (P6 item 18): the destroy
//! succeeds and the mob's database is gone from disk, in the daemon that
//! made it and in a later one that restored it. A mob a client follows
//! (a council's) is `tests/councils.rs`'s: the daemon closes the client's
//! stream on it when it is destroyed (`docs/evidence.md`, "`mob_destroy`").

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/rig.rs"]
mod rig;

use std::path::PathBuf;
use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::Attached;
use rkat_tui::ui::Effect;
use serde_json::json;

/// The prompt that makes the team (`make`) or destroys it (`destroy`).
fn script() -> Arc<ScriptedLlmClient> {
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let user = common::user_texts(messages).concat();
        let done = messages
            .iter()
            .filter(|m| matches!(m, Message::ToolResults { .. }))
            .count();
        if user.ends_with("destroy") {
            return match done {
                // A make in the same conversation came first.
                n if n == 2 || n == 0 => Step::Tool {
                    id: "d1",
                    name: "mob_destroy",
                    args: json!({"mob_id": "team"}),
                },
                _ => Step::Text("gone"),
            };
        }
        if !user.ends_with("make") {
            // The member's own turns.
            return Step::Text("ready");
        }
        match done {
            0 => Step::Tool {
                id: "m1",
                name: "mob_create",
                args: json!({"definition": {"id": "team", "profiles": {"worker": {"model": common::MODEL, "tools": {"comms": true}}}}}),
            },
            1 => Step::Tool {
                id: "s1",
                name: "mob_spawn_member",
                args: json!({"mob_id": "team", "profile": "worker", "member_id": "w1", "initial_message": "hello"}),
            },
            _ => Step::Text("made"),
        }
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
    assert_eq!(rig.turn_finished().await.result.unwrap().text, "made");
    assert!(database(&ws).exists());
    rig.send(Effect::StartTurn("destroy".into()));
    assert_eq!(rig.turn_finished().await.result.unwrap().text, "gone");
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
    assert_eq!(rig.turn_finished().await.result.unwrap().text, "made");
    // The member's first turn ended before the daemon stops.
    common::within(async {
        while !llm.recorded().iter().any(|r| r.user().contains("hello")) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    rig.quit().await;
    assert!(database(&ws).exists());

    let llm = script();
    let mut rig = Attached::start(&ws.root, &llm).await;
    reveal(&rig).await;
    rig.send(Effect::StartTurn("destroy".into()));
    assert_eq!(rig.turn_finished().await.result.unwrap().text, "gone");
    let answer = destroyed(&llm);
    assert_eq!(answer["ok"], true, "{answer}");
    assert!(!database(&ws).exists());
    rig.quit().await;
}
