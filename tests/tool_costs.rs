//! What an agent's model requests pay for tool definitions, by family
//! (`docs/evidence.md`, "Deferrable tools"): every definition rides on every
//! request, because the engine's deferred tool catalog never engages for
//! the daemon's members (Meerkat 0.9.12: the mob operator tools and
//! `memory_search` report no exact catalog, so the session's catalog is
//! inexact and stays inline). The measurements are asserted as bounds, so a
//! family that grows, and the engine change that would let tools be
//! deferred, both show here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use common::{ScriptedLlmClient, Step, Workspace};
use rig::Attached;
use rkat_tui::ui::Effect;
use serde_json::json;

/// A main agent's request carries every family's definitions, about 120 KB
/// in all (schedule and mob tools a third each, comms a fifth); its
/// session's catalog is inexact, so nothing is deferred and the catalog
/// control tools are absent. A `delegate` helper's request carries no
/// schedule or mob tools.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_s_request_carries_every_tool_definition() {
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
    rig.send(Effect::StartTurn("hello".into()));
    assert_eq!(rig.turn_finished().await.result.unwrap().text, "ok");
    let recorded = llm.recorded();
    let main = &recorded[0];
    let bytes = main.tool_bytes();
    let total = main.total_tool_bytes();
    let report = format!("{total} bytes: {bytes:?}");
    for family in [
        "builtin", "comms", "files", "memory", "mob", "notes", "schedule", "watch",
    ] {
        assert!(bytes.contains_key(family), "{family} missing: {report}");
    }
    assert!(!bytes.contains_key("catalog"), "{report}");
    assert_eq!(main.tools_of("schedule").len(), 8, "{report}");
    // The engine's families: the bulk.
    assert!(bytes["schedule"] > 30_000, "{report}");
    assert!(bytes["mob"] > 30_000, "{report}");
    assert!(bytes["comms"] > 20_000, "{report}");
    // rkat-tui's own: small, and kept so.
    assert!(
        bytes["files"] + bytes["notes"] + bytes["watch"] < 10_000,
        "{report}"
    );
    assert!(total < 130_000, "{report}");
    // Nothing deferred: the session's catalog is inexact.
    let session = rig.current_session().await;
    let view = rig.daemon.origins.view(&session).await.expect("a view");
    assert!(
        !view.exact_catalog && view.mode == meerkat_core::ToolCatalogMode::Inline,
        "the engine now reports an exact catalog for the daemon's members: \
         schedule and rare mob tools can be deferred (docs/evidence.md, \"Deferrable tools\")"
    );
    // The helper: its own request, without schedule or mob tools.
    let helper = &recorded[1];
    let helper_bytes = helper.tool_bytes();
    assert!(
        !helper_bytes.contains_key("schedule") && !helper_bytes.contains_key("mob"),
        "{helper_bytes:?}"
    );
    assert!(helper.total_tool_bytes() < 45_000, "{helper_bytes:?}");
    rig.quit().await;
}
