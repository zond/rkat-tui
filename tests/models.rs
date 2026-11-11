//! `/model` end to end through the `rkat-tui-mob` daemon with a scripted
//! LLM and an attached client: the picker shows every endpoint's live list, a bare id
//! resolves to the endpoint listing it, the switch goes through the
//! runtime's session-LLM reconfigure host and the next turn's request
//! carries the new model; the endpoint's connection and the model's profile
//! are in the realm's own config document (so `rkat` resolves the session
//! too), the endpoint is derived from the session's binding when the
//! daemon restores it, and the cost totals survive that in the session's
//! cost record. The `[thinking]` setting rides in the session's provider
//! params on create, switch and restore.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/rig.rs"]
mod rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, args_with};
use meerkat_core::ConfigStore;
use meerkat_core::ProviderTag;
use meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig;
use rig::{Attached, Setup, stored_session};
use rkat_tui::bootstrap;
use rkat_tui::endpoints::{ENDPOINT_REALM, Endpoint};
use rkat_tui::models::{EndpointListing, ModelEntry, ModelLister, ModelSource};
use rkat_tui::ui::{Effect, Msg, StatusUpdate};

/// An Anthropic-dialect gateway, defined on the command line.
fn proxy_args() -> Vec<String> {
    [
        "--set",
        "endpoints.proxy.dialect=anthropic",
        "--set",
        "endpoints.proxy.base_url=http://127.0.0.1:9",
        "--set",
        // Keys come from the environment only; `PATH` is set wherever
        // tests run, and the scripted client never sends it.
        "endpoints.proxy.api_key_env=PATH",
    ]
    .map(str::to_string)
    .to_vec()
}

/// The catalogued model the proxy also lists.
const LISTED: &str = "claude-opus-4-8";

/// The proxy lists `my-alias` (a model only it knows) and a catalogued
/// Claude model, each with a 50k window.
fn lister() -> ModelLister {
    Arc::new(|endpoints: Vec<Endpoint>| {
        Box::pin(async move {
            endpoints
                .into_iter()
                .map(|e| {
                    let models = if e.name == "proxy" {
                        ["my-alias", LISTED]
                            .into_iter()
                            .map(|id| ModelEntry {
                                id: id.into(),
                                display_name: None,
                                source: ModelSource::Provider,
                                context_window: Some(50_000),
                                max_output_tokens: Some(4_096),
                                capabilities: Default::default(),
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    (
                        e.name,
                        Ok(EndpointListing {
                            models,
                            prices: Vec::new(),
                        }),
                    )
                })
                .collect()
        })
    })
}

fn options() -> Setup {
    Setup {
        args: proxy_args(),
        lister: Some(lister()),
        ..Setup::default()
    }
}

fn is_model(msg: &Msg, endpoint: &str, model: &str) -> bool {
    matches!(msg, Msg::Status(StatusUpdate::Model { endpoint: shown, model: m }) if shown == endpoint && m == model)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_picker_switches_the_live_model_and_the_next_turn_uses_it() {
    let ws = Workspace::new();
    let realm = bootstrap::resolve_scope(&args_with(&ws.root, &[]))
        .unwrap()
        .realm_root();
    let llm = ScriptedLlmClient::new(vec![Step::Text("one"), Step::Text("two")]);
    let mut rig = Attached::start_with(&ws.root, &llm, options()).await;
    let id = rig.current_session().await;
    rig.saw(|m| is_model(m, "anthropic", common::MODEL)).await;

    rig.send(Effect::StartTurn("first".into()));
    rig.turn_finished().await;
    assert_eq!(llm.recorded()[0].model, common::MODEL);

    // The picker: the current endpoint first, its current model first; the
    // proxy's live list.
    rig.send(Effect::RequestModelPicker { refresh: false });
    let Msg::ShowModelPicker(groups) = rig.wait_for(|m| matches!(m, Msg::ShowModelPicker(_))).await
    else {
        unreachable!()
    };
    assert_eq!(groups[0].endpoint, "anthropic");
    assert_eq!(groups[0].models[0].id, common::MODEL);
    assert_eq!(groups[0].models[0].source, ModelSource::Current);
    let proxy = groups.iter().find(|g| g.endpoint == "proxy").unwrap();
    assert_eq!(proxy.models[0].id, "my-alias");

    // A bare id only the proxy lists resolves to the proxy, which speaks
    // the Anthropic dialect: a model the catalog does not know is refused
    // there, naming the way out.
    rig.send(Effect::SetModel("my-alias".into()));
    rig.wait_for(|m| {
        matches!(m, Msg::Notice { text, .. }
            if text.starts_with("model not changed") && text.contains("`openai`-dialect endpoint"))
    })
    .await;
    // A catalogued model on the proxy runs, with the proxy's window.
    rig.send(Effect::SetModel(format!("proxy:{LISTED}")));
    rig.wait_for(|m| is_model(m, "proxy", LISTED)).await;
    rig.wait_for(|m| matches!(m, Msg::Status(StatusUpdate::ContextWindow(Some(50_000)))))
        .await;
    rig.send(Effect::StartTurn("second".into()));
    rig.turn_finished().await;
    assert_eq!(llm.recorded()[1].model, LISTED);

    // The identity is the session's: model and the proxy's binding.
    let meta = stored_session(&ws.root, &id)
        .await
        .session_metadata()
        .unwrap();
    assert_eq!(meta.model, LISTED);
    let binding = meta.auth_binding.expect("the proxy's binding");
    assert_eq!(binding.realm.as_str(), ENDPOINT_REALM);
    assert_eq!(binding.binding.as_str(), "proxy");
    // The binding and the alias's profile live in the realm's own config
    // document: another surface loading it resolves the session.
    let document =
        meerkat_core::FileConfigStore::new(realm.join("config.toml"), meerkat_models::canonical())
            .get()
            .await
            .unwrap();
    let target =
        meerkat_core::connection::resolve_explicit_auth_binding_target(&document, &binding)
            .unwrap();
    assert_eq!(
        target.backend.base_url.as_deref(),
        Some("http://127.0.0.1:9")
    );
    // rkat-tui writes no `[models.<id>]` profile.
    assert!(
        document.models.custom.is_empty(),
        "{:?}",
        document.models.custom
    );

    // The daemon counted each turn under the model it ran on, pushed the
    // totals and wrote them down by the time it stopped.
    rig.saw(|m| {
        matches!(m, Msg::Status(StatusUpdate::CostTotals(cost))
            if cost.per_model.get(LISTED).is_some_and(|t| t.input == 10))
    })
    .await;
    rig.quit().await;
    let cost = rkat_tui::daemon::usage::read_record(
        &rkat_tui::daemon::usage::Account::Session(id.clone()).path(&realm),
    )
    .cost;
    let alias = cost.per_model.get(LISTED).expect("the proxy's turn");
    assert_eq!((alias.input, alias.output), (10, 5), "{cost:?}");
    assert!(cost.per_model.contains_key(common::MODEL), "{cost:?}");

    // Restored by a restarted daemon: the proxy and its model, the exact
    // totals, and the next turn runs on it.
    let llm = ScriptedLlmClient::new(vec![Step::Text("three"), Step::Text("four")]);
    let mut rig = Attached::start_with(&ws.root, &llm, options()).await;
    assert_eq!(rig.current_session().await, id);
    rig.saw(|m| is_model(m, "proxy", LISTED)).await;
    rig.saw(|m| matches!(m, Msg::Status(StatusUpdate::CostTotals(totals)) if *totals == cost))
        .await;
    rig.send(Effect::StartTurn("third".into()));
    rig.turn_finished().await;
    assert_eq!(llm.recorded()[0].model, LISTED);

    // Back to a built-in endpoint: the explicit binding is cleared.
    rig.send(Effect::SetModel("anthropic:claude-opus-4-8".into()));
    rig.wait_for(|m| is_model(m, "anthropic", "claude-opus-4-8"))
        .await;
    rig.send(Effect::StartTurn("fourth".into()));
    rig.turn_finished().await;
    assert_eq!(llm.recorded()[1].model, "claude-opus-4-8");
    let meta = stored_session(&ws.root, &id)
        .await
        .session_metadata()
        .unwrap();
    assert!(meta.auth_binding.is_none(), "{meta:?}");
    rig.quit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unresolvable_references_leave_the_model_alone() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut rig = Attached::start_with(&ws.root, &llm, options()).await;
    rig.send(Effect::SetModel("nope:model".into()));
    let msg = rig
        .wait_for(
            |m| matches!(m, Msg::Notice { text, .. } if text.starts_with("model not changed")),
        )
        .await;
    assert!(
        matches!(&msg, Msg::Notice { text, .. } if text.contains("unknown endpoint `nope`")),
        "{msg:?}"
    );
    rig.send(Effect::StartTurn("still here".into()));
    rig.turn_finished().await;
    assert_eq!(llm.recorded()[0].model, common::MODEL);
    rig.quit().await;
}

/// The `thinking` of the Anthropic tag a request carried.
fn thinking_of(recorded: &common::Recorded) -> Option<AnthropicThinkingConfig> {
    match &recorded.provider_params {
        Some(ProviderTag::Anthropic(tag)) => tag.thinking.clone(),
        _ => None,
    }
}

/// The request's tag with its `thinking` taken out.
fn without_thinking(recorded: &common::Recorded) -> Option<ProviderTag> {
    match recorded.provider_params.clone() {
        Some(ProviderTag::Anthropic(mut tag)) => {
            tag.thinking = None;
            Some(ProviderTag::Anthropic(tag)).filter(|tag| {
                *tag != ProviderTag::Anthropic(
                    meerkat_core::lifecycle::run_primitive::AnthropicProviderTag::default(),
                )
            })
        }
        other => other,
    }
}

/// [`options`] plus `thinking` settings on the command line.
fn thinking_options(settings: &[&str]) -> Setup {
    let mut options = options();
    for setting in settings {
        options.args.push("--set".to_string());
        options.args.push((*setting).to_string());
    }
    options
}

/// Run a turn on `rig` and switch it to the proxy's alias, then run
/// another.
async fn turn_switch_turn(rig: &mut Attached) {
    rig.saw(|m| is_model(m, "anthropic", common::MODEL)).await;
    rig.send(Effect::StartTurn("first".into()));
    rig.turn_finished().await;
    rig.send(Effect::SetModel(format!("proxy:{LISTED}")));
    rig.wait_for(|m| is_model(m, "proxy", LISTED)).await;
    rig.send(Effect::StartTurn("second".into()));
    rig.turn_finished().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn configured_thinking_reaches_the_request_on_create_and_after_a_switch() {
    // Configured: a budget for the catalogued start-up model by its bare id,
    // adaptive for the proxy alias by its reference.
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("one"), Step::Text("two")]);
    let mut rig = Attached::start_with(
        &ws.root,
        &llm,
        thinking_options(&[
            &format!("thinking.\"{}\".enabled=4096", common::MODEL),
            &format!("thinking.\"proxy:{LISTED}\"=adaptive"),
        ]),
    )
    .await;
    let id = rig.current_session().await;
    turn_switch_turn(&mut rig).await;
    let recorded = llm.recorded();
    assert_eq!(
        thinking_of(&recorded[0]),
        Some(AnthropicThinkingConfig::Enabled {
            budget_tokens: 4096
        })
    );
    assert_eq!(recorded[1].model, LISTED);
    assert_eq!(
        thinking_of(&recorded[1]),
        Some(AnthropicThinkingConfig::Adaptive)
    );
    // The session's identity records it. (Read from the store once the
    // daemon stopped: a second realm persistence opened in the daemon's
    // process takes over the engine's process-wide auth lease state, and
    // the daemon's next rotation finds that store gone when it is
    // dropped.)
    rig.quit().await;
    let meta = stored_session(&ws.root, &id)
        .await
        .session_metadata()
        .unwrap();
    let Some(ProviderTag::Anthropic(tag)) = meta.provider_params.and_then(|p| p.provider_tag)
    else {
        panic!("the session's provider params carry the Anthropic tag");
    };
    assert_eq!(tag.thinking, Some(AnthropicThinkingConfig::Adaptive));

    // A switch to a model without a setting clears it (the daemon
    // restored on the alias).
    let after = ScriptedLlmClient::new(vec![Step::Text("again"), Step::Text("three")]);
    let mut rig = Attached::start_with(
        &ws.root,
        &after,
        thinking_options(&[
            &format!("thinking.\"{}\".enabled=4096", common::MODEL),
            &format!("thinking.\"proxy:{LISTED}\"=adaptive"),
        ]),
    )
    .await;
    rig.saw(|m| is_model(m, "proxy", LISTED)).await;
    rig.send(Effect::StartTurn("again".into()));
    rig.turn_finished().await;
    assert_eq!(
        thinking_of(&after.recorded()[0]),
        Some(AnthropicThinkingConfig::Adaptive)
    );
    rig.send(Effect::SetModel(format!("anthropic:{}", "claude-opus-4-8")));
    rig.wait_for(|m| is_model(m, "anthropic", "claude-opus-4-8"))
        .await;
    rig.send(Effect::StartTurn("third".into()));
    rig.turn_finished().await;
    let third = &after.recorded()[1];
    assert_eq!(third.model, "claude-opus-4-8");
    assert_eq!(thinking_of(third), None, "{:?}", third.provider_params);
    rig.quit().await;

    // Absent: the requests carry no thinking, as before the setting existed.
    let ws = Workspace::new();
    let plain = ScriptedLlmClient::new(vec![Step::Text("one"), Step::Text("two")]);
    let mut rig = Attached::start_with(&ws.root, &plain, options()).await;
    turn_switch_turn(&mut rig).await;
    rig.quit().await;
    let plain = plain.recorded();
    assert_eq!(plain[1].model, LISTED);
    assert_eq!(
        thinking_of(&plain[0]),
        None,
        "{:?}",
        plain[0].provider_params
    );
    assert_eq!(
        thinking_of(&plain[1]),
        None,
        "{:?}",
        plain[1].provider_params
    );
    // Only the thinking differs from the plain requests.
    assert_eq!(without_thinking(&recorded[0]), without_thinking(&plain[0]));
    assert_eq!(without_thinking(&recorded[1]), without_thinking(&plain[1]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_follows_the_thinking_configured_now() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("one")]);
    let mut rig = Attached::start_with(&ws.root, &llm, options()).await;
    rig.send(Effect::StartTurn("first".into()));
    rig.turn_finished().await;
    rig.quit().await;
    assert_eq!(thinking_of(&llm.recorded()[0]), None);

    // Configured since: the restored conversation thinks adaptively.
    let llm = ScriptedLlmClient::new(vec![Step::Text("two")]);
    let mut rig = Attached::start_with(
        &ws.root,
        &llm,
        thinking_options(&[&format!("thinking.\"{}\"=adaptive", common::MODEL)]),
    )
    .await;
    rig.send(Effect::StartTurn("second".into()));
    rig.turn_finished().await;
    assert_eq!(
        thinking_of(&llm.recorded()[0]),
        Some(AnthropicThinkingConfig::Adaptive)
    );
    rig.quit().await;

    // No longer configured: the setting is gone again.
    let llm = ScriptedLlmClient::new(vec![Step::Text("three")]);
    let mut rig = Attached::start_with(&ws.root, &llm, options()).await;
    rig.send(Effect::StartTurn("third".into()));
    rig.turn_finished().await;
    assert_eq!(thinking_of(&llm.recorded()[0]), None);
    rig.quit().await;
}

/// The proxy lists the catalogued [`common::MODEL`] with a 50k window.
fn catalogued_lister() -> ModelLister {
    Arc::new(|endpoints: Vec<Endpoint>| {
        Box::pin(async move {
            endpoints
                .into_iter()
                .map(|e| {
                    let models = vec![ModelEntry {
                        id: common::MODEL.into(),
                        display_name: None,
                        source: ModelSource::Provider,
                        context_window: Some(50_000),
                        max_output_tokens: Some(4_096),
                        capabilities: rkat_tui::models::ModelCapabilities::default(),
                    }];
                    (
                        e.name,
                        Ok(EndpointListing {
                            models,
                            prices: Vec::new(),
                        }),
                    )
                })
                .collect()
        })
    })
}

/// Start on `model` in auto compaction.
fn window_options(model: &str) -> Setup {
    let mut args = proxy_args();
    args.extend(["--model", model, "--set", "auto.compact=auto"].map(str::to_string));
    Setup {
        args,
        lister: Some(catalogued_lister()),
        ..Setup::default()
    }
}

/// A configured endpoint is the authority for its models' facts: a
/// gateway serving a catalogued id with a 50k window has the gauge (and
/// so `/context`) show 50k and the agent's threshold follow it, not
/// of the catalog's window, although the model was named explicitly (its
/// list is read at start). The same id on the built-in endpoint keeps the
/// catalog's window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_configured_endpoint_s_listing_decides_the_window_and_the_threshold() {
    let catalog = rkat_tui::endpoints::registry_context_window(
        &meerkat_core::Config::default(),
        meerkat_core::Provider::Anthropic,
        common::MODEL,
    )
    .expect("a catalogued window");
    assert_ne!(catalog, 50_000);

    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let reference = format!("proxy:{}", common::MODEL);
    let setup = window_options(&reference);
    let policy = bootstrap::read_layers(&setup.command_line(&ws.root))
        .unwrap()
        .layered
        .settings
        .compaction_policy(None);
    let threshold = |window| policy.trigger(Some(window)).threshold_override();
    assert_ne!(threshold(50_000), threshold(catalog));
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    rig.saw(|m| is_model(m, "proxy", common::MODEL)).await;
    rig.saw(|m| matches!(m, Msg::Status(StatusUpdate::ContextWindow(Some(50_000)))))
        .await;
    assert_eq!(rig.daemon.compact_threshold, threshold(50_000));
    rig.quit().await;

    let ws = Workspace::new();
    let reference = format!("anthropic:{}", common::MODEL);
    let mut rig = Attached::start_with(&ws.root, &llm, window_options(&reference)).await;
    rig.saw(|m| is_model(m, "anthropic", common::MODEL)).await;
    rig.saw(|m| matches!(m, Msg::Status(StatusUpdate::ContextWindow(Some(w))) if *w == catalog))
        .await;
    assert_eq!(rig.daemon.compact_threshold, threshold(catalog));
    rig.quit().await;
}

/// A start on a model the catalog does not know, on an Anthropic-dialect
/// gateway, is refused with the reason and the way out (an
/// `openai`-dialect endpoint), and the realm config gains no profile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uncatalogued_model_on_a_gateway_is_refused_at_start() {
    let ws = Workspace::new();
    let realm = bootstrap::resolve_scope(&args_with(&ws.root, &[]))
        .unwrap()
        .realm_root();
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let mut args = proxy_args();
    args.extend(["--model".to_string(), "proxy:my-alias".to_string()]);
    let setup = Setup {
        args,
        lister: Some(lister()),
        ..Setup::default()
    };
    let refused = Attached::try_start_with(&ws.root, &llm, setup).await;
    let Err(err) = refused else {
        panic!("an uncatalogued model started on an Anthropic-dialect gateway");
    };
    let message = err.to_string();
    assert!(
        message.contains("`my-alias` is not in Meerkat's model catalog"),
        "{message}"
    );
    assert!(message.contains("`openai`-dialect endpoint"), "{message}");
    let document = std::fs::read_to_string(realm.join("config.toml")).unwrap_or_default();
    assert!(!document.contains("[models."), "{document}");
    assert!(llm.recorded().is_empty());
}

/// `/model` before a restored agent's first turn switches it (the daemon
/// wakes the dormant member first), and
/// the turn after runs on the new model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_agent_switches_models_before_its_first_turn() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("one")]);
    let mut rig = Attached::start_with(&ws.root, &llm, options()).await;
    let id = rig.current_session().await;
    rig.send(Effect::StartTurn("first".into()));
    rig.turn_finished().await;
    rig.quit().await;

    let llm = ScriptedLlmClient::new(vec![Step::Text("two")]);
    let mut rig = Attached::start_with(&ws.root, &llm, options()).await;
    assert_eq!(rig.current_session().await, id);
    rig.saw(|m| is_model(m, "anthropic", common::MODEL)).await;
    rig.send(Effect::SetModel("anthropic:claude-opus-4-8".into()));
    let msg = rig
        .wait_for(|m| {
            is_model(m, "anthropic", "claude-opus-4-8")
                || matches!(m, Msg::Notice { text, .. } if text.starts_with("model not changed"))
        })
        .await;
    assert!(is_model(&msg, "anthropic", "claude-opus-4-8"), "{msg:?}");
    rig.send(Effect::StartTurn("second".into()));
    rig.turn_finished().await;
    assert_eq!(llm.recorded()[0].model, "claude-opus-4-8");
    rig.quit().await;
}
