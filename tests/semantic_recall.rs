//! Native semantic memory, end to end through the `rkat-tui-mob` daemon
//! with a scripted LLM and an attached client: the agent is offered
//! `memory_search` (and every builtin it is offered is classified by the
//! approval policy);
//! a fact native auto-compaction discarded from the context is found
//! through `memory_search`, so the daemon wires the memory store the
//! compaction indexes into and the tool reads. That compaction drops the
//! fact from the context and indexes it is the engine's own property,
//! tested upstream (`meerkat` `compaction_ownership_gaps.rs`, the setup
//! `compact_during_run_n` its tests share).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/rig.rs"]
mod rig;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::Message;
use rig::{Attached, Setup};
use rkat_tui::approval::catalog::ToolOrigin;
use rkat_tui::approval::policy::{
    Action, DELEGATION_TOOLS, PolicyContext, READ_ONLY_BUILTINS, REVIEWED_BUILTINS, Tier,
    classify_tier,
};
use rkat_tui::approval::rules::RuleSet;
use rkat_tui::memory::NOTE_TOOL_NAMES;
use rkat_tui::paths::ProjectRoot;
use rkat_tui::ui::Effect;
use serde_json::json;

const SECRET: &str = "the zebra pancake recipe uses saffron";

/// The TUI's own file tools.
const FILE_TOOLS: &[&str] = &["read_file", "write_file", "edit_file", "glob", "grep"];

fn tool_results(messages: &[Message]) -> String {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResults { results, .. } => Some(
                results
                    .iter()
                    .map(meerkat_core::types::ToolResult::text_content)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn auto_rig(ws: &Workspace, llm: &std::sync::Arc<ScriptedLlmClient>) -> Attached {
    let setup = Setup::args(&[
        "--auto-compact",
        "--set",
        "auto.compact_warn_percent=5",
        "--set",
        "auto.compact_auto_percent=5",
    ]);
    Attached::start_with(&ws.root, llm, setup).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_offered_builtin_is_classified_and_memory_search_is_tier_one() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut rig = Attached::start(&ws.root, &llm).await;
    // Every hidden tool revealed, so the request offers every tool.
    let session = rig.current_session().await;
    let hidden: Vec<String> = rig
        .daemon
        .deferral()
        .deferrable
        .all()
        .iter()
        .map(|def| def.name.to_string())
        .collect();
    assert!(hidden.len() >= 26, "{hidden:?}");
    let hidden: Vec<&str> = hidden.iter().map(String::as_str).collect();
    rig.daemon.reveal(&session, &hidden).await;
    rig.send(Effect::StartTurn("hi".into()));
    let _ = rig.turn_finished().await;
    let tools = llm.recorded()[0].tools.clone();
    assert!(tools.iter().any(|t| t == "memory_search"), "{tools:?}");
    assert!(
        tools.iter().any(|t| t == "meerkat_schedule_create"),
        "{tools:?}"
    );

    let root = ProjectRoot::new(&ws.root).unwrap();
    let ctx = PolicyContext {
        root: &root,
        trust_project_builds: true,
        posix_shell: true,
    };
    let rules = RuleSet::default();
    let mut unclassified = Vec::new();
    for name in &tools {
        let known = READ_ONLY_BUILTINS.contains(&name.as_str())
            || REVIEWED_BUILTINS.contains(&name.as_str())
            || DELEGATION_TOOLS.contains(&name.as_str())
            || FILE_TOOLS.contains(&name.as_str())
            || NOTE_TOOL_NAMES.contains(&name.as_str())
            || [rkat_tui::tools::WATCH_PATHS, rkat_tui::tools::UNWATCH].contains(&name.as_str())
            || name == rkat_tui::daemon::deferral::reveal::TOOL_SEARCH
            // Mutating mob tools (the read-only ones are listed).
            || name.starts_with("mob_");
        if !known {
            unclassified.push(name.clone());
        }
        if READ_ONLY_BUILTINS.contains(&name.as_str()) {
            let args = serde_json::value::RawValue::from_string("{}".to_string()).unwrap();
            let origin = ToolOrigin::native(meerkat_core::ToolSourceKind::Builtin);
            let tier = classify_tier(&Action::parse(name, &args, Some(&origin)), &rules, &ctx).tier;
            assert_eq!(tier, Tier::AlwaysAllow, "{name}");
        }
    }
    // The search is tier 1, judged with rkat-tui's own tools.
    let args = serde_json::value::RawValue::from_string("{}".to_string()).unwrap();
    let origin = ToolOrigin::tui(
        rkat_tui::daemon::deferral::reveal::TOOL_SEARCH_SOURCE,
        rkat_tui::daemon::deferral::reveal::TOOL_SEARCH,
    );
    let action = Action::parse(
        rkat_tui::daemon::deferral::reveal::TOOL_SEARCH,
        &args,
        Some(&origin),
    );
    assert_eq!(classify_tier(&action, &rules, &ctx).tier, Tier::AlwaysAllow);
    assert!(
        unclassified.is_empty(),
        "classify these offered tools in READ_ONLY_BUILTINS or REVIEWED_BUILTINS: {unclassified:?}"
    );
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compacted_fact_is_found_with_memory_search() {
    let ws = Workspace::new();
    let mut script = vec![Step::Text("noted")];
    script.extend(vec![Step::Text("ok"); 4]);
    script.push(Step::TextUsage("big", 60_000));
    // Next turn: the summary, then the answer.
    script.push(Step::Text("SUMMARY without details"));
    script.push(Step::Text("compacted"));
    // Then the model searches its memory.
    script.push(Step::Tool {
        id: "call-1",
        name: "memory_search",
        args: json!({ "query": "zebra pancake recipe" }),
    });
    script.push(Step::Text("saffron"));
    let llm = ScriptedLlmClient::new(script);
    let mut rig = auto_rig(&ws, &llm).await;

    rig.send(Effect::StartTurn(format!("remember this: {SECRET}")));
    rig.turn_finished().await.expect("turn succeeds");
    for i in 0..5 {
        rig.send(Effect::StartTurn(format!("turn {i}")));
        rig.turn_finished().await.expect("turn succeeds");
    }
    rig.send(Effect::StartTurn("compact".into()));
    rig.turn_finished().await.expect("turn succeeds");

    rig.send(Effect::StartTurn("what was the recipe?".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.expect("turn succeeds").text, "saffron");
    let answer = llm.recorded().last().unwrap().clone();
    let found = tool_results(&answer.messages);
    assert!(found.contains("saffron"), "memory_search found: {found}");
    rig.quit().await;
}
