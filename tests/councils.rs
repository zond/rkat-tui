//! Councils from the daemon's agent (P6 item 17): the engine seats a
//! council's participants in a temporary mob it creates for an owner
//! session no session here has; the gate records which session convened it
//! (`rkat_tui::mobs::council`), so the participants work for the agent:
//! gated for it, followed by its client, and the council completes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use std::sync::Arc;
use std::time::Duration;

use common::{ScriptedLlmClient, Step, Workspace, within};
use meerkat_core::{Message, StreamScopeFrame};
use rig::{Attached, ScriptedClassifier, Setup};
use rkat_tui::ui::{Effect, Msg};
use serde_json::json;

/// The member a scoped message is attributed to.
fn member_of(msg: &Msg) -> Option<String> {
    let Msg::Scoped(ev) = msg else { return None };
    ev.scope_path.iter().rev().find_map(|f| match f {
        StreamScopeFrame::MobMember { agent_identity, .. } => Some(agent_identity.clone()),
        _ => None,
    })
}

/// The body of the background job notice in `messages` naming `tool`.
fn job_notice(messages: &[Message], tool: &str) -> Option<String> {
    messages.iter().find_map(|m| match m {
        Message::SystemNotice(notice) => notice.blocks.iter().find_map(|block| match block {
            meerkat_core::SystemNoticeBlock::BackgroundJob {
                display_name,
                detail,
                ..
            } if display_name.as_deref() == Some(tool) => Some(format!(
                "{:?} {}",
                notice.body,
                detail.clone().unwrap_or_default()
            )),
            _ => None,
        }),
        _ => None,
    })
}

/// The agent makes a team of two, convenes them; each participant makes a
/// reviewed call, then gives its view; the council's end reaches the agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_two_member_council_completes_with_its_participants_as_helpers() {
    let ws = Workspace::new();
    let script = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let user = common::user_texts(messages).concat();
        let results = messages
            .iter()
            .filter(|m| matches!(m, Message::ToolResults { .. }))
            .count();
        if user.contains("tabs or spaces") {
            // A participant: one reviewed call, then its view.
            return if results == 0 {
                Step::Tool {
                    id: "p1",
                    name: "shell",
                    args: json!({"command": "touch council.ran"}),
                }
            } else {
                Step::Text("my view")
            };
        }
        if user != "convene" {
            return Step::Text("ready");
        }
        match results {
            0 => Step::Tool {
                id: "m1",
                name: "mob_create",
                args: json!({"definition": {"id": "team", "profiles": {"worker": {"model": common::MODEL, "tools": {"comms": true, "shell": true}}}}}),
            },
            1 => Step::Tool {
                id: "s1",
                name: "mob_spawn_member",
                args: json!({"mob_id": "team", "profile": "worker", "member_id": "w1"}),
            },
            2 => Step::Tool {
                id: "s2",
                name: "mob_spawn_member",
                args: json!({"mob_id": "team", "profile": "worker", "member_id": "w2"}),
            },
            3 => Step::Tool {
                id: "c1",
                name: "council",
                args: json!({
                    "topic": "tabs or spaces",
                    "participants": [
                        {"mob_id": "team", "member_id": "w1", "role": "pro"},
                        {"mob_id": "team", "member_id": "w2", "role": "con"}
                    ],
                    "max_rounds": 1,
                    "timeout_seconds": 60
                }),
            },
            _ => Step::Text("done"),
        }
    }));
    let llm = ScriptedLlmClient::new(vec![script; 60]);
    let classifier = ScriptedClassifier::allowing(Vec::new());
    let setup = Setup::default().classifier(Arc::clone(&classifier) as _);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    // The mob tools the script calls (`tests/deferral.rs` covers finding
    // them).
    let session = rig.current_session().await;
    rig.daemon
        .reveal(&session, &["mob_create", "mob_spawn_member", "council"])
        .await;
    rig.send(Effect::StartTurn("convene".into()));
    rig.turn_finished().await;

    // The council's end reaches the agent, and it did not fail.
    let notice = within(async {
        loop {
            if let Some(notice) = llm
                .recorded()
                .iter()
                .find_map(|r| job_notice(&r.messages, "council"))
            {
                return notice;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(!notice.contains("(failed)"), "{notice}");
    assert!(!notice.contains("helper refused"), "{notice}");
    assert!(notice.contains("my view"), "{notice}");
    // It cleaned up after itself: its temporary mob is destroyed.
    assert!(
        notice.contains("\"temporary_mob_destroyed\":true"),
        "{notice}"
    );

    // The participants' reviewed call went through the agent's gate (the
    // classifier reviewed it against the agent's transcript, whose user
    // said "convene"; the second participant's identical call is the
    // owner's cached allow) and ran.
    let reviewed: Vec<String> = classifier
        .seen()
        .into_iter()
        .filter(|(tool, args, _)| tool == "shell" && args["command"] == "touch council.ran")
        .map(|(_, _, transcript)| transcript)
        .collect();
    assert_eq!(reviewed.len(), 1, "{:?}", classifier.tools());
    assert!(reviewed[0].contains("convene"), "{}", reviewed[0]);
    assert!(ws.root.join("council.ran").exists());

    // The client followed the participants as helpers.
    for participant in ["council-p0", "council-p1"] {
        rig.saw(|m| member_of(m).as_deref() == Some(participant))
            .await;
    }
    rig.quit().await;
}

/// A council mob the gate never admitted keeps the engine's owner session,
/// which no session here has: its participants are refused, as any member
/// of a mob made for a foreign session is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_council_mob_nobody_convened_here_gets_no_participant() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("never")]);
    let daemon = rig::Running::start(&ws.root, &llm).await;
    let foreign = meerkat_core::SessionId::new();
    let mut definition = meerkat_mob::MobDefinition::implicit("unused", common::MODEL);
    definition.id = meerkat_mob::MobId::from("council--agent-0123456789abcdef");
    let mob = within(
        daemon
            .state()
            .mob_create_definition_with_owner_bridge_session(definition, foreign, false, false),
    )
    .await
    .unwrap();
    let handle = within(daemon.state().handle_for(&mob)).await.unwrap();
    let refused =
        within(handle.spawn_spec(meerkat_mob::SpawnMemberSpec::new("delegate", "council-p0")))
            .await;
    let Err(err) = refused else {
        panic!("the spawn is refused");
    };
    assert!(
        err.to_string()
            .contains("is neither a session of this TUI nor a helper"),
        "{err}"
    );
    assert!(daemon.registry().live().is_empty());
    assert!(llm.recorded().is_empty(), "no member ran");
    drop(handle);
    daemon.stop().await;
}
