//! `agent.web_search`, end to end through the `rkat-tui-mob` daemon with a
//! scripted LLM: `false` builds the agent and a fresh helper with the
//! engine's web search category off (their stored sessions say
//! `Disable`), and no request of either carries a provider-native search
//! tool or a `web_search` tool. With the default, the category stays the
//! engine's (`Inherit`) and the requests carry no search tool either:
//! every member is built with rkat-tui's dispatch admission, under which
//! the engine sends no provider-native tool (`docs/evidence.md`,
//! "agent.web_search").

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use common::{ScriptedLlmClient, Step, Workspace};
use meerkat_core::lifecycle::run_primitive::ProviderTag;
use meerkat_core::{Message, ToolCategoryOverride};
use rig::{Client, Running};
use rkat_tui::ui::Effect;
use std::sync::Arc;

/// Whether `recorded` asks the provider for its own web search, or offers
/// a `web_search` tool.
fn searches(recorded: &common::Recorded) -> bool {
    let native = match &recorded.provider_params {
        Some(ProviderTag::Anthropic(tag)) => tag.web_search.is_some(),
        Some(ProviderTag::OpenAi(tag)) => tag.web_search.is_some(),
        Some(ProviderTag::Gemini(tag)) => tag.google_search.is_some(),
        _ => false,
    };
    native || recorded.tools.iter().any(|tool| tool == "web_search")
}

/// The agent's turn assigns the fresh helper `fresh`; the helper answers.
fn script() -> Arc<ScriptedLlmClient> {
    let step = Step::Dynamic(Arc::new(|messages: &[Message]| {
        if common::is_helper(messages, "FRESH-TASK") {
            return Step::Text("fresh done");
        }
        if !format!("{messages:?}").contains("\"a1\"") {
            return common::assign("a1", "fresh", "FRESH-TASK");
        }
        Step::Text("ok")
    }));
    ScriptedLlmClient::new(vec![step; 10])
}

/// Run the agent's turn and its helper under `extra` arguments; the
/// stored web search category of the agent's session and the helper's,
/// and every request.
async fn run_with(
    extra: &[&str],
) -> (
    ToolCategoryOverride,
    ToolCategoryOverride,
    Vec<common::Recorded>,
) {
    let ws = Workspace::new();
    let llm = script();
    let daemon = Running::start_with(
        &common::args_with(&ws.root, extra),
        Running::options(&ws.root, &llm),
    )
    .await;
    let client = Client::attach(&ws.root).await;
    client.send(Effect::StartTurn("go".into()));
    let registry = daemon.registry();
    let key = rkat_tui::mobs::HelperKey::new(
        meerkat_mob::MobId::from("tui"),
        meerkat_mob::AgentIdentity::from("fresh"),
    );
    let helper = common::poll(async || registry.member_session(&key)).await;
    common::until(|| common::parent_told(&llm, "FRESH-TASK", "fresh")).await;
    daemon.idle(&daemon.agent).await;
    let agent = daemon.session.clone();
    client.quit().await;
    daemon.stop().await;
    let category = async |id| {
        rig::stored_session(&ws.root, id)
            .await
            .session_metadata()
            .unwrap()
            .tooling
            .web_search
    };
    (
        category(&agent).await,
        category(&helper).await,
        llm.recorded(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_search_off_builds_the_agent_and_a_fresh_helper_without_it() {
    let (agent, helper, recorded) = run_with(&["--set", "agent.web_search=false"]).await;
    assert_eq!(agent, ToolCategoryOverride::Disable);
    assert_eq!(helper, ToolCategoryOverride::Disable);
    assert!(
        recorded
            .iter()
            .any(|r| common::is_helper(&r.messages, "FRESH-TASK")),
        "the helper ran"
    );
    for request in &recorded {
        assert!(
            !searches(request),
            "{:?} {:?}",
            request.provider_params,
            request.tools
        );
    }
}

/// The daemon restores its agent on the same conversation: built with the
/// default, it is turned off by a restart with `false`, and a restart
/// with the default again keeps it off (the engine restores the session's
/// own `Disable` when the build leaves the category `Inherit`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_conversation_is_turned_off_and_stays_off() {
    let ws = Workspace::new();
    let mut id = None;
    for (extra, expected) in [
        (&[][..], ToolCategoryOverride::Inherit),
        (
            &["--set", "agent.web_search=false"][..],
            ToolCategoryOverride::Disable,
        ),
        (&[][..], ToolCategoryOverride::Disable),
    ] {
        let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
        let mut rig = rig::Attached::start_with(&ws.root, &llm, rig::Setup::args(extra)).await;
        let session = rig.current_session().await;
        assert_eq!(*id.get_or_insert_with(|| session.clone()), session);
        rig.send(Effect::StartTurn("hi".into()));
        let _ = rig.turn_finished().await;
        assert!(!searches(&llm.recorded()[0]));
        rig.quit().await;
        let stored = rig::stored_session(&ws.root, &session)
            .await
            .session_metadata()
            .unwrap()
            .tooling
            .web_search;
        assert_eq!(stored, expected, "{extra:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn by_default_the_category_is_the_engine_s_and_no_request_searches() {
    let (agent, helper, recorded) = run_with(&[]).await;
    // Never `Enable`, which would ask the engine for its fallback tool.
    assert_eq!(agent, ToolCategoryOverride::Inherit);
    assert_eq!(helper, ToolCategoryOverride::Inherit);
    for request in &recorded {
        assert!(
            !searches(request),
            "{:?} {:?}",
            request.provider_params,
            request.tools
        );
    }
}
