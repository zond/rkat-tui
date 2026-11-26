//! `endpoints.<name>.models`, end to end on the engine's own clients
//! against a fake OpenAI-dialect endpoint (`common/fake_openai.rs`) that
//! lists two models, of which the launcher's `--replace` list allows one:
//! every place a model is chosen or used refuses the other with the
//! setting's name, and no refused request reaches the endpoint (its
//! recorded model calls are asserted). The key is read from
//! `CARGO_MANIFEST_DIR`, which Cargo sets for every test process; no test
//! reaches the network.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::fake;
use crate::rig;

use std::sync::Arc;
use std::time::Duration;

use common::{ScriptedLlmClient, Workspace};
use fake::{FakeEndpoint, FakeModel, Reply};
use rig::{Attached, Setup};
use rkat_tui::endpoints::Endpoint;
use rkat_tui::models::ModelLister;
use rkat_tui::ui::{Effect, Msg, StatusUpdate, TurnFailure};
use serde_json::json;

/// The model the list allows.
const ALLOWED: &str = "local-coder";
/// A model the endpoint lists and the list does not allow.
const FORBIDDEN: &str = "claude-opus-5-5";
/// The variable the endpoint's key is read from.
const KEY_ENV: &str = "CARGO_MANIFEST_DIR";
/// The refusal's words.
const SETTING: &str = "endpoints.gateway.models";

fn models() -> Vec<FakeModel> {
    [FORBIDDEN, ALLOWED]
        .into_iter()
        .map(|id| FakeModel {
            id,
            max_input_tokens: 64_000,
            max_output_tokens: 2_048,
            input_cost: 1e-6,
            output_cost: 2e-6,
            supports_reasoning: false,
        })
        .collect()
}

/// Lists only the configured endpoints, over HTTP.
fn configured_lister() -> ModelLister {
    Arc::new(|endpoints: Vec<Endpoint>| {
        Box::pin(async move {
            let configured: Vec<Endpoint> =
                endpoints.into_iter().filter(|e| !e.is_builtin()).collect();
            rkat_tui::models::list_all_endpoints(&configured, Duration::from_secs(10)).await
        })
    })
}

/// The launcher's command line: the endpoint `gateway` at `fake`, its
/// list `allowed` given with `--replace`, then `extra`.
fn setup(fake: &FakeEndpoint, allowed: &[&str], extra: &[&str]) -> Setup {
    let list = format!(
        "{SETTING}=[{}]",
        allowed
            .iter()
            .map(|id| format!("{id:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut args: Vec<String> = vec![
        "--endpoint-def".into(),
        format!("gateway=openai={},key_env={KEY_ENV}", fake.base_url),
        "--replace".into(),
        list,
    ];
    args.extend(extra.iter().map(ToString::to_string));
    Setup {
        args,
        lister: Some(configured_lister()),
        engine_clients: true,
        ..Setup::default()
    }
}

fn is_model(msg: &Msg, model: &str) -> bool {
    matches!(msg, Msg::Status(StatusUpdate::Model { endpoint, model: m }) if endpoint == "gateway" && m == model)
}

fn notice_with(msg: &Msg, text: &str) -> bool {
    matches!(msg, Msg::Notice { text: shown, .. } if shown.contains(text))
}

/// The start-up model (`--model`, here also the classifier's
/// `--classifier-model`) outside the list stops the start with the
/// refusal; nothing reaches the endpoint. An allowed model runs. (A bare
/// id only the listed endpoint could take, and the list's first as the
/// default: `endpoints::tests::references_meet_the_list`,
/// `bootstrap::tests::the_start_up_plan_takes_the_allowed_models`; a
/// built-in endpoint a key in the environment adds would change which
/// endpoint takes it here.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_up_model_outside_the_list_stops_the_start() {
    let fake = FakeEndpoint::start(models(), vec![Reply::Text("hello")]).await;
    let llm = ScriptedLlmClient::new(Vec::new());
    for extra in [
        vec!["--model".to_string(), format!("gateway:{FORBIDDEN}")],
        vec![
            "--model".to_string(),
            format!("gateway:{ALLOWED}"),
            "--classifier-model".to_string(),
            format!("gateway:{FORBIDDEN}"),
        ],
    ] {
        let ws = Workspace::new();
        let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
        let mut setup = setup(&fake, &[ALLOWED], &extra);
        setup.llm_classifier = true;
        let Err(err) = Attached::try_start_with(&ws.root, &llm, setup).await else {
            panic!("{extra:?}: the daemon started");
        };
        let text = format!("{err:#}");
        assert!(
            text.contains(&format!(
                "model `{FORBIDDEN}` is not allowed on endpoint `gateway`"
            )) && text.contains(&format!("{SETTING} allows only {ALLOWED}")),
            "{extra:?}: {text}"
        );
    }
    assert!(fake.turns().is_empty());
    // The allowed model runs.
    let ws = Workspace::new();
    let mut rig = Attached::start_with(
        &ws.root,
        &llm,
        setup(&fake, &[ALLOWED], &["--model", "gateway:local-coder"]),
    )
    .await;
    rig.saw(|m| is_model(m, ALLOWED)).await;
    rig.send(Effect::StartTurn("hi".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "hello");
    let turns = fake.turns();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].body["model"], ALLOWED);
    rig.quit().await;
}

/// `/model`: a typed `gateway:<id>`, a bare id the endpoint lists and a
/// bare id nothing knows are each refused naming the setting (the bare
/// one does not fall through to the only endpoint); the daemon refuses
/// `rkat/set_model` of the forbidden model itself, whatever a client
/// checked; the classifier's model too (`rkat/set_setting`); a key given
/// with `--replace` is not changed by `/set`. The agent keeps its model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_switches_outside_the_list_are_refused() {
    let fake = FakeEndpoint::start(models(), vec![Reply::Text("still here")]).await;
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(Vec::new());
    let extra = [
        "--model",
        "gateway:local-coder",
        "--replace",
        "auto.compact_warn_percent=70",
    ];
    let mut rig = Attached::start_with(&ws.root, &llm, setup(&fake, &[ALLOWED], &extra)).await;
    rig.saw(|m| is_model(m, ALLOWED)).await;
    rig.send(Effect::SetModel(format!("gateway:{FORBIDDEN}")));
    let refusal = format!(
        "model not changed: model `{FORBIDDEN}` is not allowed on endpoint `gateway`: {SETTING} allows only {ALLOWED}"
    );
    rig.wait_for(|m| notice_with(m, &refusal)).await;
    // A bare id no list has does not fall through to the listed endpoint
    // (with no other endpoint the refusal is the list's; a built-in
    // endpoint a key in the environment adds makes it ambiguous, also a
    // refusal: `endpoints::tests::references_meet_the_list` pins both).
    rig.send(Effect::SetModel("unknown-model".into()));
    rig.wait_for(|m| notice_with(m, "model not changed:") && notice_with(m, "unknown-model"))
        .await;
    // The daemon decides, whatever the client checked.
    let connection = rig.daemon.connect_bound().await;
    let gateway = rkat_tui::endpoints::Endpoint::configured(
        "gateway",
        rkat_tui::endpoints::WireDialect::OpenAi,
        None,
        rkat_tui::endpoints::SecretString::default(),
        KEY_ENV,
        0,
        rkat_tui::endpoints::ApiStyle::default(),
    );
    let err = connection
        .call(
            "rkat/set_model",
            json!({"model": FORBIDDEN, "provider": "openai", "auth_binding": gateway.binding()}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(SETTING) && err.contains("the model was not changed"),
        "{err}"
    );
    let answer = connection
        .call(
            "rkat/set_setting",
            json!({"key": "auto.classifier_model", "value": format!("gateway:{FORBIDDEN}")}),
        )
        .await
        .unwrap();
    assert_eq!(answer["ok"], false, "{answer}");
    assert!(
        answer["text"].as_str().unwrap().contains(SETTING),
        "{answer}"
    );
    let answer = connection
        .call(
            "rkat/set_setting",
            json!({"key": "auto.compact_warn_percent", "value": "30"}),
        )
        .await
        .unwrap();
    assert_eq!(answer["ok"], false, "{answer}");
    assert!(
        answer["text"].as_str().unwrap().contains("--replace"),
        "{answer}"
    );
    drop(connection);
    rig.send(Effect::StartTurn("hi".into()));
    rig.turn_finished().await.unwrap();
    let turns = fake.turns();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].body["model"], ALLOWED);
    rig.quit().await;
}

/// A helper on a model the list does not allow: `assign` is refused with
/// the setting's words and what to do; no helper's model call reaches
/// the endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_on_a_model_outside_the_list_is_refused_with_what_to_do() {
    let fake = FakeEndpoint::start(
        models(),
        vec![
            Reply::Tool {
                name: "assign",
                args: json!({"name": "scout", "task": "say hi", "model": FORBIDDEN}),
            },
            Reply::Text("done"),
        ],
    )
    .await;
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(Vec::new());
    let mut rig = Attached::start_with(
        &ws.root,
        &llm,
        setup(&fake, &[ALLOWED], &["--model", "gateway:local-coder"]),
    )
    .await;
    rig.send(Effect::StartTurn("start one".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "done");
    let turns = fake.turns();
    assert_eq!(turns.len(), 2, "no helper call: {turns:#?}");
    for turn in &turns {
        assert_eq!(turn.body["model"], ALLOWED);
    }
    let result = turns[1].body.to_string();
    assert!(
        result.contains(SETTING) && result.contains("Call again naming one of those models"),
        "{result}"
    );
    rig.quit().await;
}

/// A conversation saved on a model the list allowed then, opened by a
/// daemon whose list no longer does: it opens (its transcript reads),
/// but a run is refused with the setting's words in the agents' client
/// decorator, the backstop below every other check, and no request
/// reaches the endpoint; `/btw` on it is refused too; after `/model`
/// picks an allowed model it runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stored_model_now_outside_the_list_is_refused_at_its_next_request() {
    let fake = FakeEndpoint::start(models(), Vec::new()).await;
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(Vec::new());
    let both = [ALLOWED, FORBIDDEN];
    let mut rig = Attached::start_with(
        &ws.root,
        &llm,
        setup(&fake, &both, &["--model", &format!("gateway:{FORBIDDEN}")]),
    )
    .await;
    rig.saw(|m| is_model(m, FORBIDDEN)).await;
    rig.send(Effect::StartTurn("remember the marker 7f3a".into()));
    rig.turn_finished().await.unwrap();
    let id = rig.current_session().await;
    rig.quit().await;
    assert_eq!(fake.turns().len(), 1);

    let mut rig = Attached::start_with(
        &ws.root,
        &llm,
        setup(
            &fake,
            &[ALLOWED],
            &["--model", &format!("gateway:{ALLOWED}")],
        ),
    )
    .await;
    assert_eq!(rig.current_session().await, id);
    rig.saw(|m| is_model(m, FORBIDDEN)).await;
    // The transcript reads.
    let messages = rig.messages().await;
    assert!(format!("{messages:?}").contains("7f3a"));
    rig.send(Effect::StartTurn("go on".into()));
    let failure = rig.turn_finished().await.unwrap_err();
    let TurnFailure::Failed(text) = failure else {
        panic!("{failure:?}");
    };
    assert!(
        text.contains(&format!(
            "model `{FORBIDDEN}` is not allowed on endpoint `gateway`"
        )) && text.contains("/model"),
        "{text}"
    );
    rig.send(Effect::Btw {
        question: "what was the marker?".into(),
        running_tool: None,
        current_turn: None,
    });
    let answer = rig.wait_for(|m| matches!(m, Msg::BtwAnswered(_))).await;
    let Msg::BtwAnswered(Err(why)) = answer else {
        panic!("{answer:?}");
    };
    assert!(why.contains(SETTING), "{why}");
    assert_eq!(
        fake.turns().len(),
        1,
        "no refused request reached the endpoint"
    );
    // An allowed model runs it again.
    rig.send(Effect::SetModel(format!("gateway:{ALLOWED}")));
    rig.wait_for(|m| is_model(m, ALLOWED)).await;
    rig.send(Effect::StartTurn("go on".into()));
    rig.turn_finished().await.unwrap();
    let turns = fake.turns();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1].body["model"], ALLOWED);
    rig.quit().await;
}
