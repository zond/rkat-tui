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
#[path = "common/attach.rs"]
mod rig;

use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, args_with};
use meerkat_core::ConfigStore;
use meerkat_core::ProviderTag;
use meerkat_core::Usage;
use meerkat_core::lifecycle::run_primitive::AnthropicThinkingConfig;
use rig::{Attached, Setup, stored_session};
use rkat_tui::bootstrap;
use rkat_tui::endpoints::{ENDPOINT_REALM, Endpoint};
use rkat_tui::models::{EndpointListing, ModelEntry, ModelLister, ModelSource};
use rkat_tui::ui::{Effect, Msg, SessionCost, StatusUpdate};
use rkat_tui::workbench::cost_store::load_cost;

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

/// The proxy lists `my-alias` (a model only it knows) with a 50k window.
fn lister() -> ModelLister {
    Arc::new(|endpoints: Vec<Endpoint>| {
        Box::pin(async move {
            endpoints
                .into_iter()
                .map(|e| {
                    let models = if e.name == "proxy" {
                        vec![ModelEntry {
                            id: "my-alias".into(),
                            display_name: Some("My Alias".into()),
                            source: ModelSource::Provider,
                            context_window: Some(50_000),
                            max_output_tokens: Some(4_096),
                            capabilities: Default::default(),
                        }]
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
    matches!(msg, Msg::Status(StatusUpdate::Model { provider, model: m }) if provider == endpoint && m == model)
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

    // A bare id only the proxy lists resolves to the proxy.
    rig.send(Effect::SetModel("my-alias".into()));
    rig.wait_for(|m| is_model(m, "proxy", "my-alias")).await;
    rig.wait_for(|m| matches!(m, Msg::Status(StatusUpdate::ContextWindow(Some(50_000)))))
        .await;
    rig.send(Effect::StartTurn("second".into()));
    rig.turn_finished().await;
    assert_eq!(llm.recorded()[1].model, "my-alias");

    // The identity is the session's: model and the proxy's binding.
    let meta = stored_session(&ws.root, &id)
        .await
        .session_metadata()
        .unwrap();
    assert_eq!(meta.model, "my-alias");
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
    let profile = &document.models.custom["my-alias"];
    assert_eq!(profile.context_window, Some(50_000));

    // The UI's cost totals land in the sidecar.
    let mut cost = SessionCost::default();
    cost.record(
        "my-alias",
        &Usage {
            input_tokens: 1_000,
            output_tokens: 500,
            ..Usage::default()
        },
    );
    rig.send(Effect::CostSnapshot(cost.clone()));
    rig.quit().await;
    assert_eq!(load_cost(&realm, &id.to_string()), Some(cost.clone()));

    // Restored by a restarted daemon: the proxy and its model, the exact
    // totals, and the next turn runs on it.
    let llm = ScriptedLlmClient::new(vec![Step::Text("three"), Step::Text("four")]);
    let mut rig = Attached::start_with(&ws.root, &llm, options()).await;
    assert_eq!(rig.current_session().await, id);
    rig.saw(|m| is_model(m, "proxy", "my-alias")).await;
    rig.saw(|m| matches!(m, Msg::Status(StatusUpdate::CostTotals(totals)) if *totals == cost))
        .await;
    rig.send(Effect::StartTurn("third".into()));
    rig.turn_finished().await;
    assert_eq!(llm.recorded()[0].model, "my-alias");

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
    rig.send(Effect::SetModel("proxy:my-alias".into()));
    rig.wait_for(|m| is_model(m, "proxy", "my-alias")).await;
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
            "thinking.\"proxy:my-alias\"=adaptive",
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
    assert_eq!(recorded[1].model, "my-alias");
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
            "thinking.\"proxy:my-alias\"=adaptive",
        ]),
    )
    .await;
    rig.saw(|m| is_model(m, "proxy", "my-alias")).await;
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
    assert_eq!(plain[1].model, "my-alias");
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
/// so `/context`) show 50k and the orchestrator's threshold follow it, not
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

/// Section 8 item 4 on the rig: a realm config that profiles a model the
/// catalog defines (a catalog addition since rkat-tui wrote it) starts.
/// rkat-tui's own entry (recorded in the realm's `tui_models.json`) is
/// dropped with a notice; the same entry the user wrote refuses the start
/// with a typed error naming it, instead of a daemon that cannot write its
/// realm config for an opaque reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_profile_the_catalog_now_defines_is_dropped_or_named() {
    let ws = Workspace::new();
    let realm = bootstrap::resolve_scope(&args_with(&ws.root, &[]))
        .unwrap()
        .realm_root();
    std::fs::create_dir_all(&realm).unwrap();
    let shadowing = format!(
        "[models.\"{}\"]\nprovider = \"anthropic\"\ncontext_window = 50000\n",
        common::MODEL
    );
    std::fs::write(realm.join("config.toml"), &shadowing).unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let refused = Attached::try_start_with(&ws.root, &llm, Setup::default()).await;
    let Err(err) = refused else {
        panic!("a user's entry shadowing the catalog was dropped");
    };
    let message = err.to_string();
    assert!(
        message.contains(&format!("[models.\"{}\"]", common::MODEL)),
        "{message}"
    );
    assert_eq!(
        std::fs::read_to_string(realm.join("config.toml")).unwrap(),
        shadowing
    );

    // rkat-tui wrote it: dropped, and the agent runs.
    std::fs::write(
        realm.join(rkat_tui::endpoints::OWNED_PROFILES_FILE),
        format!("{{\"profiles\": [\"{}\"]}}", common::MODEL),
    )
    .unwrap();
    let mut rig = Attached::start_with(&ws.root, &llm, Setup::default()).await;
    let entry = format!("[models.\"{}\"]", common::MODEL);
    rig.saw(|m| matches!(m, Msg::Notice { text, .. } if text.contains(&entry)))
        .await;
    rig.send(Effect::StartTurn("hi".into()));
    rig.turn_finished().await;
    rig.quit().await;
    let document = std::fs::read_to_string(realm.join("config.toml")).unwrap();
    assert!(!document.contains("[models."), "{document}");
}
