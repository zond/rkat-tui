//! What an agent's model requests pay for tool definitions, by family
//! (`docs/evidence.md`, "Deferrable tools" and "Search and reveal"): every
//! declared definition rides on every request. The engine's deferred tool
//! catalog never engages for the daemon's members (Meerkat 0.9.12 and
//! 0.9.13: the mob operator tools and `memory_search` report no exact
//! catalog, so the session's catalog is inexact and stays inline), so
//! rkat-tui hides the schedule tools and the agent mob tools but
//! `delegate` itself until a `tool_search` reveals them
//! (`src/daemon/deferral`). The measurements are asserted as bounds, so a
//! family that grows, and the engine change that would let tools be
//! deferred natively (lukacf/meerkat#2180), both show here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/attach.rs"]
mod rig;

use common::{ScriptedLlmClient, Step, Workspace};
use rig::Attached;
use rkat_tui::ui::Effect;
use serde_json::json;

/// A fresh conversation's request carries the direct tools and the search
/// tool, about 55 KB (122 KB before rkat-tui's own deferral): no schedule
/// tool and of the agent mob tools only `delegate`; what is hidden (the
/// eight schedule tools, 18 mob tools, about 67 KB) `/context` reports as
/// hidden. Its session's catalog is inexact, so the engine defers nothing
/// itself and its catalog control tools are absent. A `delegate` helper's
/// request carries neither hidden family nor the search tool.
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
        "builtin", "catalog", "comms", "files", "memory", "mob", "notes", "watch",
    ] {
        assert!(bytes.contains_key(family), "{family} missing: {report}");
    }
    // The search tool, small; the engine's own control tools absent.
    assert_eq!(main.tools_of("catalog"), ["tool_search"], "{report}");
    assert!(bytes["catalog"] < 1_500, "{report}");
    // Hidden: no schedule tool; of the agent mob tools, `delegate` (the
    // mob operator tools stay, the engine mounts them).
    assert!(!bytes.contains_key("schedule"), "{report}");
    let deferral = rig.daemon.deferral();
    let declared_hidden: Vec<_> = main
        .tools
        .iter()
        .filter(|name| deferral.deferrable.contains(name))
        .collect();
    assert!(declared_hidden.is_empty(), "{declared_hidden:?}: {report}");
    assert!(main.tools.iter().any(|name| name == "delegate"), "{report}");
    assert!(bytes["mob"] < 12_000, "{report}");
    assert!(bytes["comms"] > 20_000, "{report}");
    // rkat-tui's own: small, and kept so.
    assert!(
        bytes["files"] + bytes["notes"] + bytes["watch"] < 10_000,
        "{report}"
    );
    assert!(total < 56_000, "{report}");
    // What is hidden: the eight schedule tools and 18 mob tools.
    let session = rig.current_session().await;
    let hidden = deferral.hidden(&session);
    let hidden_report = format!("{hidden:?}");
    let of = |family| {
        hidden
            .iter()
            .find(|size| size.family == family)
            .copied()
            .expect(&hidden_report)
    };
    use rkat_tui::daemon::context::ToolFamily;
    assert_eq!(of(ToolFamily::Schedule).tools, 8, "{hidden_report}");
    assert!(of(ToolFamily::Schedule).bytes > 30_000, "{hidden_report}");
    assert_eq!(of(ToolFamily::Mob).tools, 18, "{hidden_report}");
    assert!(of(ToolFamily::Mob).bytes > 25_000, "{hidden_report}");
    let hidden_bytes: usize = hidden.iter().map(|size| size.bytes).sum();
    // The persona's hint lines and the search tool cost a small fraction
    // of what they hide.
    let hints = rkat_tui::daemon::deferral::PERSONA.len() + bytes["catalog"];
    assert!(hints * 20 < hidden_bytes, "{hints} vs {hidden_bytes}");
    eprintln!(
        "fresh request {total} bytes, hidden {hidden_bytes} bytes, hints {hints} bytes: {report}"
    );
    // `/context` reports the same sizes, by family, from the daemon's own
    // read of the session's tool definitions.
    let sizes = common::within(async {
        loop {
            if let Some(sizes) = rig.daemon.origins.sizes(&session) {
                return sizes;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    for size in &sizes {
        assert_eq!(
            bytes.get(size.family.name()),
            Some(&size.bytes),
            "{:?}: {report}",
            size.family
        );
        assert_eq!(
            size.tools,
            main.tools_of(size.family.name()).len(),
            "{report}"
        );
    }
    assert_eq!(sizes.len(), bytes.len(), "{sizes:?}: {report}");
    // Nothing deferred: the session's catalog is inexact.
    let view = rig.daemon.origins.view(&session).await.expect("a view");
    assert!(
        !view.exact_catalog && view.mode == meerkat_core::ToolCatalogMode::Inline,
        "the engine now reports an exact catalog for the daemon's members: \
         schedule and rare mob tools can be deferred (docs/evidence.md, \"Deferrable tools\")"
    );
    // The helper: its own request, without schedule or mob tools and
    // without the search tool.
    let helper = &recorded[1];
    let helper_bytes = helper.tool_bytes();
    assert!(
        !helper_bytes.contains_key("schedule")
            && !helper_bytes.contains_key("mob")
            && !helper_bytes.contains_key("catalog"),
        "{helper_bytes:?}"
    );
    assert!(helper.total_tool_bytes() < 45_000, "{helper_bytes:?}");
    eprintln!("helper request {} bytes", helper.total_tool_bytes());
    rig.quit().await;
}
