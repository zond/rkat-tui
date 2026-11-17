//! Peer requests between the agents and helpers. On the engine release in
//! use a session that sent a peer request stops for good when a response
//! without a result arrives (lukacf/meerkat PR #2402's changelog entry;
//! shown on this rig before the guard: the requester's next input was
//! refused "has no committed runtime attachment … (unregister_not_started)"
//! and the daemon's clean stop failed "terminal completion finalization
//! lost the exact durable CAS"). So neither `send_request` nor
//! `send_response` is offered to an agent or a helper: they ask and answer
//! with `send_message`. A call of either is refused; the helper goes on
//! taking messages.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::{Running, ScriptedClassifier, Setup};
use serde_json::json;

fn tool(id: &'static str, name: &'static str, args: serde_json::Value) -> Step {
    Step::Tool { id, name, args }
}

fn result_of(messages: &[Message], id: &str) -> Option<(String, bool)> {
    messages.iter().find_map(|m| match m {
        Message::ToolResults { results, .. } => results
            .iter()
            .find(|r| r.tool_use_id == id)
            .map(|r| (r.text_content(), r.is_error)),
        _ => None,
    })
}

/// Whether `text` is the refusal of a call of `name`: the members' tool
/// policy's (the tool is not offered) or the gate's, which says to use
/// `send_message`.
fn refused(text: &str, name: &str) -> bool {
    text.contains(&format!("'{name}' is not allowed by policy"))
        || text.contains(rkat_tui::approval::policy::PEER_REQUEST_UNAVAILABLE)
}

/// The helper `w` tries a request to its parent, and the parent answers
/// `w` with a well-formed response (a result per the contract): neither
/// tool is offered to either, and both calls are refused; then `w` takes a
/// next message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn neither_peer_request_tool_is_offered_and_the_helper_runs_on() {
    let ws = Workspace::new();
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        let user = common::user_texts(messages).concat();
        let all = format!("{messages:?}");
        if user.contains("TASK-OF-W") {
            if all.contains("AGAIN") {
                return Step::Text("W-RAN-AGAIN");
            }
            if all.contains("\"r1\"") {
                return Step::Text("W-DONE");
            }
            if all.contains("\"p1\"") {
                let parent = crate::assign::peer_id(messages, "main-").unwrap_or_default();
                return tool(
                    "r1",
                    "send_request",
                    json!({"peer_id": parent, "intent": "checksum_token", "params": {"subject": "s"}, "handling_mode": "queue"}),
                );
            }
            return tool("p1", "peers", json!({}));
        }
        if all.contains("\"x1\"") {
            return Step::Text("parent done");
        }
        if all.contains("\"s1\"") {
            let w = crate::assign::peer_id(messages, "main/w").unwrap_or_default();
            if w.is_empty() {
                return tool("p0", "peers", json!({}));
            }
            return tool(
                "x1",
                "send_response",
                json!({"peer_id": w, "in_reply_to": "01a1ea66-0000-7000-8000-000000000000", "status": "completed", "result": {"request_intent": "checksum_token", "request_subject": "s", "token": "t"}}),
            );
        }
        tool(
            "s1",
            "spawn_member",
            json!({"profile": "main", "member_id": "w", "initial_message": "TASK-OF-W", "auto_wire_parent": true}),
        )
    }));
    let llm = ScriptedLlmClient::new(vec![step; 40]);
    let classifier = ScriptedClassifier::allowing(Vec::new());
    let setup = Setup::default().classifier(Arc::clone(&classifier) as _);
    let daemon =
        Running::start_with(&setup.command_line(&ws.root), setup.options(&ws.root, &llm)).await;
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, "go").await;
    let found = |id: &'static str| {
        let llm = Arc::clone(&llm);
        async move {
            common::poll(async || {
                llm.recorded()
                    .iter()
                    .find_map(|r| result_of(&r.messages, id))
            })
            .await
        }
    };
    let (request, error) = found("r1").await;
    assert!(error && refused(&request, "send_request"), "{request}");
    let (response, error) = found("x1").await;
    assert!(error && refused(&response, "send_response"), "{response}");
    // Neither the agent nor its helper is offered either tool; both are
    // offered `send_message`.
    let recorded = llm.recorded();
    for (who, is_helper) in [("the agent", false), ("the helper", true)] {
        let requests: Vec<_> = recorded
            .iter()
            .filter(|r| r.user().contains("TASK-OF-W") == is_helper)
            .collect();
        assert!(!requests.is_empty(), "no request of {who}");
        for r in requests {
            let offered: Vec<String> = r.defs.iter().map(|d| d.name.to_string()).collect();
            assert!(
                offered.iter().any(|n| n == "send_message"),
                "{who}: {offered:?}"
            );
            assert!(
                !offered
                    .iter()
                    .any(|n| n == "send_request" || n == "send_response"),
                "{who}: {offered:?}"
            );
        }
    }
    // Nothing reached the classifier: both were refused before review.
    assert!(
        !classifier
            .tools()
            .iter()
            .any(|t| t == "send_request" || t == "send_response"),
        "{:?}",
        classifier.tools()
    );
    // `w` runs on: its next model request holds the next message.
    connection
        .call(
            "mob/member_send",
            json!({"mob_id": "tui", "agent_identity": "w", "content": "AGAIN"}),
        )
        .await
        .unwrap();
    common::until(|| {
        llm.recorded().iter().any(|r| {
            r.user().contains("TASK-OF-W") && format!("{:?}", r.messages).contains("AGAIN")
        })
    })
    .await;
    daemon.idle(&daemon.agent).await;
    drop(connection);
    daemon.stop().await;
}
