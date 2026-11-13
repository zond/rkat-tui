//! The daemon pushes what a client used to poll for: another client's
//! conversation switch (`rkat/switched`) and a mob that hosts a helper
//! (`rkat/mob`). The rig counts the engine requests the clients send
//! through the daemon's proxy, so a poll would show.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::time::Duration;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::{AgentEvent, StreamScopeFrame};
use rig::{Attached, Client, Running};
use rkat_tui::ui::{Effect, Msg};
use serde_json::json;

/// Longer than the poll intervals the client used to have (a member status
/// every second, a mob listing every 250 ms): a poll would be counted.
const QUIET: Duration = Duration::from_millis(1_500);

/// Client B shows A's `/new` from the daemon's push; neither client reads
/// the agent's member status while nothing happens, before or after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_clients_new_is_shown_without_polling() {
    const STATUS: &str = "mob/member_status";
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut a = Client::attach(&ws.root).await;
    let mut b = Client::attach(&ws.root).await;
    // What the attaches read (each follows the agents' mob once) is done
    // within the first quiet spell; the second counts nothing new.
    tokio::time::sleep(QUIET).await;
    let idle = daemon.forwarded(STATUS);
    tokio::time::sleep(QUIET).await;
    assert_eq!(daemon.forwarded(STATUS), idle, "nothing polls while idle");

    a.send(Effect::NewSession);
    a.wait_for(|m| matches!(m, Msg::Notice { text, .. } if text.starts_with("a new conversation")))
        .await;
    b.wait_for(
        |m| matches!(m, Msg::Notice { text, .. } if text.starts_with("another client switched")),
    )
    .await;
    let now = daemon.current_session().await;
    assert_ne!(now, daemon.session);
    tokio::time::sleep(QUIET).await;
    let switched = daemon.forwarded(STATUS);
    tokio::time::sleep(QUIET).await;
    assert_eq!(daemon.forwarded(STATUS), switched, "nothing polls after");
    b.quit().await;
    a.quit().await;
    daemon.stop().await;
}

/// A `delegate` helper's events are shown from its first one (the
/// daemon pushes its implicit mob, the client backfills what came before
/// its stream opened), and its start makes the client list no mobs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delegate_helper_is_followed_from_its_first_event_without_listing_mobs() {
    const LIST: &str = "mob/list";
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "d1",
            name: "delegate",
            args: json!({
                "member_id": "h1",
                "task": "say hi",
                "result_label": "result",
                "max_text_bytes": 4000,
            }),
        },
        Step::Text("helper done"),
        Step::Text("ok"),
    ]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    // The attach lists the mobs once.
    common::until(|| rig.daemon.forwarded(LIST) > 0).await;
    let listed = rig.daemon.forwarded(LIST);
    assert_eq!(listed, 1);
    rig.send(Effect::StartTurn("delegate it".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "ok");
    /// The event of the helper `h1` that `m` shows, if any.
    fn of_helper(m: &Msg) -> Option<&AgentEvent> {
        match m {
            Msg::Scoped(ev) => ev
                .scope_path
                .iter()
                .any(|frame| {
                    matches!(frame, StreamScopeFrame::MobMember { agent_identity, .. } if agent_identity == "h1")
                })
                .then_some(&ev.event),
            _ => None,
        }
    }
    rig.saw(|m| matches!(of_helper(m), Some(AgentEvent::RunStarted { .. })))
        .await;
    rig.saw(|m| {
        matches!(of_helper(m), Some(AgentEvent::TextComplete { content, .. }) if content == "helper done")
    })
    .await;
    assert_eq!(rig.daemon.forwarded(LIST), listed, "no mob listing");
    rig.quit().await;
}
