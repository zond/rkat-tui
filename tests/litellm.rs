//! A LiteLLM-shaped OpenAI-dialect endpoint on the engine's own clients:
//! the daemon reaches a configured OpenAI-dialect endpoint as one of the
//! engine's `self_hosted` servers, each of its models as the alias
//! `<endpoint>:<model>` whose facts all come from the endpoint
//! (`/v1/models` and `/model/info`). Every test here runs the real HTTP
//! clients against a fake endpoint in the test process
//! (`common/fake_openai.rs`); none uses a provider key, the user's token
//! store or the network. The key is read from `CARGO_MANIFEST_DIR`, which
//! Cargo sets for every test process and the confined shell's environment
//! never carries.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/fake_openai.rs"]
mod fake;
#[path = "common/attach.rs"]
mod rig;

use std::sync::Arc;
use std::time::Duration;

use common::{ScriptedLlmClient, Step, Workspace};
use fake::{FakeEndpoint, FakeModel, Reply};
use meerkat_core::{ConfigStore, Provider};
use rig::{Attached, Setup, stored_session};
use rkat_tui::bootstrap;
use rkat_tui::endpoints::{ENDPOINT_REALM, Endpoint};
use rkat_tui::models::ModelLister;
use rkat_tui::pricing::PriceSource;
use rkat_tui::ui::{Effect, Msg, StatusUpdate};
use serde_json::json;

/// The catalogued Claude id the endpoint serves (the catalog gives it
/// another window and Anthropic's request shaping).
const CLAUDE: &str = "claude-opus-5-5";
/// A model only the endpoint knows.
const LOCAL: &str = "local-coder";
/// The variable the endpoint's key is read from.
const KEY_ENV: &str = "CARGO_MANIFEST_DIR";

fn key() -> String {
    std::env::var(KEY_ENV).expect("Cargo sets it for tests")
}

fn models() -> Vec<FakeModel> {
    vec![
        FakeModel {
            id: CLAUDE,
            max_input_tokens: 50_000,
            max_output_tokens: 4_096,
            input_cost: 3e-6,
            output_cost: 15e-6,
            supports_reasoning: true,
        },
        FakeModel {
            id: LOCAL,
            // Room for the agent's tool definitions (the schedule tools
            // alone are about 9k tokens), or the first turn compacts.
            max_input_tokens: 64_000,
            max_output_tokens: 2_048,
            input_cost: 1e-6,
            output_cost: 2e-6,
            supports_reasoning: false,
        },
    ]
}

/// Lists only the configured endpoints, over HTTP (a built-in endpoint a
/// developer's environment may have a key for is never asked).
fn configured_lister() -> ModelLister {
    Arc::new(|endpoints: Vec<Endpoint>| {
        Box::pin(async move {
            let configured: Vec<Endpoint> = endpoints.into_iter().filter(|e| !e.builtin).collect();
            rkat_tui::models::list_all_endpoints(&configured, Duration::from_secs(10)).await
        })
    })
}

/// A daemon on the engine's own clients with the endpoint `litellm` at
/// `fake`, started on `litellm:<model>`.
fn setup(fake: &FakeEndpoint, model: &str, extra: &[&str]) -> Setup {
    let mut args: Vec<String> = [
        "--set",
        "endpoints.litellm.dialect=openai",
        "--set",
        &format!("endpoints.litellm.base_url={}", fake.base_url),
        "--set",
        &format!("endpoints.litellm.api_key_env={KEY_ENV}"),
        "--model",
        &format!("litellm:{model}"),
    ]
    .map(|arg| arg.to_string())
    .to_vec();
    args.extend(extra.iter().map(ToString::to_string));
    Setup {
        args,
        lister: Some(configured_lister()),
        engine_clients: true,
        ..Setup::default()
    }
}

fn is_model(msg: &Msg, endpoint: &str, model: &str) -> bool {
    matches!(msg, Msg::Status(StatusUpdate::Model { provider, model: m }) if provider == endpoint && m == model)
}

/// The realm's config document as another surface reads it.
async fn document(ws: &Workspace) -> (meerkat_core::Config, String) {
    let path = bootstrap::resolve_scope(&common::args_with(&ws.root, &[]))
        .unwrap()
        .realm_root()
        .join("config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (meerkat_core::Config::default(), String::new());
    };
    let config = meerkat_core::FileConfigStore::new(path, meerkat_models::canonical())
        .get()
        .await
        .unwrap();
    (config, text)
}

/// Whether `value` holds the key `name` anywhere.
fn has_key(value: &serde_json::Value, name: &str) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            map.contains_key(name) || map.values().any(|v| has_key(v, name))
        }
        serde_json::Value::Array(items) => items.iter().any(|v| has_key(v, name)),
        _ => false,
    }
}

/// The section 8 rig test: a LiteLLM endpoint serving the catalogued
/// `claude-opus-5-5` runs it on the endpoint's facts, not the catalog's.
/// The request names the model as the endpoint does and carries the
/// endpoint's key, no Anthropic beta header and no thinking field; the
/// gauge and the orchestrator's threshold follow the endpoint's 50k
/// window; the cost uses the endpoint's prices; the realm document holds
/// the alias and no `[models."claude-opus-5-5"]`, and names the key's
/// variable, never the key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_litellm_endpoint_runs_a_catalogued_claude_id_on_its_own_facts() {
    let fake = FakeEndpoint::start(models(), vec![Reply::Text("hello from the proxy")]).await;
    let ws = Workspace::new();
    let setup = setup(&fake, CLAUDE, &["--set", "auto.compact=auto"]);
    let policy = bootstrap::read_layers(&setup.command_line(&ws.root))
        .unwrap()
        .layered
        .settings
        .compaction_policy(None);
    let catalog = rkat_tui::endpoints::registry_context_window(
        &meerkat_core::Config::default(),
        Provider::Anthropic,
        CLAUDE,
    )
    .expect("a catalogued window");
    assert_ne!(catalog, 50_000);
    let llm = ScriptedLlmClient::new(Vec::new());
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    rig.saw(|m| is_model(m, "litellm", CLAUDE)).await;
    rig.saw(|m| matches!(m, Msg::Status(StatusUpdate::ContextWindow(Some(50_000)))))
        .await;
    assert_eq!(
        rig.daemon.compact_threshold,
        policy.trigger(Some(50_000)).threshold_override()
    );

    rig.send(Effect::StartTurn("hi".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.unwrap().text, "hello from the proxy");
    assert!(llm.recorded().is_empty(), "the engine's own client ran");
    let turns = fake.turns();
    assert_eq!(turns.len(), 1);
    let request = &turns[0];
    assert_eq!(request.path, "/v1/responses");
    assert_eq!(request.body["model"], CLAUDE);
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {}", key()).as_str())
    );
    assert!(request.header("anthropic-beta").is_none(), "{request:?}");
    assert!(!has_key(&request.body, "thinking"), "{}", request.body);

    // The endpoint's prices.
    let has_prices =
        |m: &Msg| matches!(m, Msg::Status(StatusUpdate::Prices(p)) if p.lookup(CLAUDE).is_some());
    rig.saw(has_prices).await;
    let prices = rig
        .seen
        .iter()
        .rev()
        .find_map(|m| match m {
            Msg::Status(StatusUpdate::Prices(p)) if has_prices(m) => Some(Arc::clone(p)),
            _ => None,
        })
        .unwrap();
    let (price, source) = prices.lookup(CLAUDE).unwrap();
    assert_eq!(source, PriceSource::Endpoint);
    assert!((price.input - 3.0).abs() < 1e-9);
    // The turn's usage is accounted to the alias, which the client
    // prices as the endpoint's model, as the cost readout does.
    let usage = rig
        .seen
        .iter()
        .find_map(|m| match m {
            Msg::Agent(env) => match &env.payload {
                meerkat_core::AgentEvent::TurnCompleted {
                    usage: Some(usage), ..
                } => Some(usage.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("the turn's usage");
    let accounted = usage.accounting().model.clone();
    assert_eq!(accounted, format!("litellm:{CLAUDE}"));
    let mut cost = rkat_tui::ui::SessionCost::default();
    cost.record(&accounted, &usage);
    let (usd, _) = cost.cost_usd(&prices);
    assert!(
        (usd - (1_000.0 * 3e-6 + 100.0 * 15e-6)).abs() < 1e-9,
        "{usd}"
    );

    let id = rig.current_session().await;
    rig.quit().await;
    let (config, text) = document(&ws).await;
    assert!(!config.models.custom.contains_key(CLAUDE), "{text}");
    let alias = &config.self_hosted.models[&format!("litellm:{CLAUDE}")];
    assert_eq!(alias.remote_model, CLAUDE);
    assert_eq!(alias.context_window, Some(50_000));
    assert_eq!(alias.max_output_tokens, Some(4_096));
    assert!(alias.supports_reasoning);
    // Every model the endpoint lists has its alias.
    assert!(
        config
            .self_hosted
            .models
            .contains_key(&format!("litellm:{LOCAL}"))
    );
    assert_eq!(
        config.self_hosted.servers["litellm"].base_url,
        fake.base_url
    );
    assert!(text.contains(KEY_ENV), "{text}");
    assert!(!text.contains(&key()), "the document never holds the key");
    let meta = stored_session(&ws.root, &id)
        .await
        .session_metadata()
        .unwrap();
    assert_eq!(meta.model, format!("litellm:{CLAUDE}"));
    let binding = meta.auth_binding.expect("the endpoint's binding");
    assert_eq!(binding.realm.as_str(), ENDPOINT_REALM);
    assert_eq!(binding.binding.as_str(), "litellm");
}

/// The negative case: the same id on the built-in `anthropic` endpoint
/// keeps the catalog's row (its window, the Anthropic provider, no
/// binding, no alias).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_built_in_endpoint_keeps_the_catalog_row() {
    let ws = Workspace::new();
    let catalog = rkat_tui::endpoints::registry_context_window(
        &meerkat_core::Config::default(),
        Provider::Anthropic,
        CLAUDE,
    )
    .unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("ok")]);
    let setup = Setup::args(&["--model", &format!("anthropic:{CLAUDE}")]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup).await;
    rig.saw(|m| is_model(m, "anthropic", CLAUDE)).await;
    rig.saw(|m| matches!(m, Msg::Status(StatusUpdate::ContextWindow(Some(w))) if *w == catalog))
        .await;
    rig.send(Effect::StartTurn("hi".into()));
    rig.turn_finished().await;
    assert_eq!(llm.recorded()[0].model, CLAUDE);
    let id = rig.current_session().await;
    rig.quit().await;
    let meta = stored_session(&ws.root, &id)
        .await
        .session_metadata()
        .unwrap();
    assert_eq!(meta.model, CLAUDE);
    assert_eq!(meta.provider, Provider::Anthropic);
    assert!(meta.auth_binding.is_none());
    let (config, _) = document(&ws).await;
    assert!(config.self_hosted.models.is_empty());
    assert!(!config.models.custom.contains_key(CLAUDE));
}

/// Probe (b): the live member switches from an alias to a built-in
/// provider's model and back (the built-in `openai` endpoint's realm
/// binding points at the same fake), and a restarted daemon resumes the
/// alias and runs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_live_member_switches_between_an_alias_and_a_built_in_model() {
    let fake = FakeEndpoint::start(models(), Vec::new()).await;
    let ws = Workspace::new();
    let scope = bootstrap::resolve_scope(&common::args_with(&ws.root, &[])).unwrap();
    std::fs::create_dir_all(scope.realm_root()).unwrap();
    // A binding for the built-in `openai` endpoint at the root of every
    // realm chain (the hot swap resolves a built-in provider's default
    // there; no provider key is in the environment).
    let realm = "global";
    std::fs::write(
        scope.realm_root().join("config.toml"),
        format!(
            r#"[realm.{realm}.backend.fake-openai]
provider = "openai"
backend_kind = "openai_api"
base_url = "{base}"

[realm.{realm}.auth.fake-openai]
provider = "openai"
auth_method = "api_key"
source = {{ kind = "env", env = "{KEY_ENV}" }}

[realm.{realm}.binding.fake-openai]
backend_profile = "fake-openai"
auth_profile = "fake-openai"
provider_default = true
"#,
            base = fake.base_url
        ),
    )
    .unwrap();
    let llm = ScriptedLlmClient::new(Vec::new());
    let mut rig = Attached::start_with(&ws.root, &llm, setup(&fake, LOCAL, &[])).await;
    rig.saw(|m| is_model(m, "litellm", LOCAL)).await;
    rig.send(Effect::StartTurn("one".into()));
    rig.turn_finished().await.result.unwrap();

    rig.send(Effect::SetModel("openai:gpt-5.5".into()));
    rig.wait_for(|m| is_model(m, "openai", "gpt-5.5")).await;
    rig.send(Effect::StartTurn("two".into()));
    rig.turn_finished().await.result.unwrap();

    rig.send(Effect::SetModel(format!("litellm:{CLAUDE}")));
    rig.wait_for(|m| is_model(m, "litellm", CLAUDE)).await;
    rig.send(Effect::StartTurn("three".into()));
    rig.turn_finished().await.result.unwrap();

    let models: Vec<String> = fake
        .turns()
        .iter()
        .map(|t| t.body["model"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(models, [LOCAL, "gpt-5.5", CLAUDE]);
    let id = rig.current_session().await;
    let meta = stored_session(&ws.root, &id)
        .await
        .session_metadata()
        .unwrap();
    assert_eq!(meta.model, format!("litellm:{CLAUDE}"));
    assert_eq!(meta.provider, Provider::SelfHosted);
    rig.quit().await;

    // A restarted daemon resumes the conversation on the alias.
    let mut rig = Attached::start_with(&ws.root, &llm, setup(&fake, LOCAL, &[])).await;
    assert_eq!(rig.current_session().await, id);
    rig.saw(|m| is_model(m, "litellm", CLAUDE)).await;
    rig.send(Effect::StartTurn("four".into()));
    rig.turn_finished().await.result.unwrap();
    let last = fake.turns().pop().unwrap();
    assert_eq!(last.body["model"], CLAUDE);
    rig.quit().await;
}

/// Probe (c): a `delegate` helper of an agent on an alias inherits it,
/// through the owner's endpoint: its request names the endpoint's model and
/// carries the endpoint's key, and its session is on the alias with the
/// owner's binding.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_helper_inherits_the_alias_through_the_owner_s_endpoint() {
    let fake = FakeEndpoint::start(
        models(),
        vec![
            Reply::Tool {
                name: "delegate",
                args: json!({
                    "member_id": "scout",
                    "task": "say hi",
                    "result_label": "result",
                    "max_text_bytes": 4000,
                }),
            },
            Reply::Text("hi from the helper"),
            Reply::Text("done"),
        ],
    )
    .await;
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(Vec::new());
    let mut rig = Attached::start_with(&ws.root, &llm, setup(&fake, CLAUDE, &[])).await;
    rig.send(Effect::StartTurn("delegate".into()));
    let outcome = rig.turn_finished().await;
    assert_eq!(outcome.result.unwrap().text, "done");
    let turns = fake.turns();
    assert_eq!(turns.len(), 3, "{turns:#?}");
    for turn in &turns {
        assert_eq!(turn.body["model"], CLAUDE);
        assert_eq!(
            turn.header("authorization"),
            Some(format!("Bearer {}", key()).as_str())
        );
    }
    rig.quit().await;
}

/// What one streamed model call of `client` yields: the text, the
/// reasoning, the tool calls (name, arguments).
async fn stream_once(
    client: &Arc<dyn meerkat::LlmClient>,
    model: &str,
) -> (String, String, Vec<(String, serde_json::Value)>) {
    use futures::StreamExt;
    let mut request = meerkat::LlmRequest::new(
        model,
        vec![meerkat_core::Message::User(
            meerkat_core::UserMessage::text("hi"),
        )],
    );
    request.tools = vec![Arc::new(meerkat_core::ToolDef::new(
        "read_file",
        "Read a file",
        json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
    ))];
    let mut stream = client.stream(&request);
    let (mut text, mut reasoning, mut tools) = (String::new(), String::new(), Vec::new());
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            meerkat::LlmEvent::TextDelta { delta, .. } => text.push_str(&delta),
            meerkat::LlmEvent::ReasoningDelta { delta } => reasoning.push_str(&delta),
            meerkat::LlmEvent::ToolCallComplete { name, args, .. } => {
                tools.push((name, serde_json::to_value(&args).unwrap()));
            }
            _ => {}
        }
    }
    (text, reasoning, tools)
}

/// Probe (a) and (e) at the engine's client: the realm document rkat-tui
/// projects for a self-hosted endpoint builds the OpenAI-compatible client
/// with the key from the auth binding's environment variable (which the
/// confined shell's environment never carries), in either of the engine's
/// API styles, chosen by the endpoint's `api_style` setting on the same
/// configured base URL (the server's root, without `/v1`); the Responses
/// API (the default, what its OpenAI dialect always spoke) and Chat
/// Completions both stream text, reasoning and tool calls, and name the
/// endpoint's model on the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_api_styles_stream_text_reasoning_and_tool_calls() {
    use rkat_tui::endpoints::{
        ApiStyle, EndpointProjection, SecretString, WireDialect, alias_entry,
    };
    assert!(
        !rkat_tui::bootstrap::confinement::confined_environment(std::env::vars_os())
            .contains_key(KEY_ENV),
        "the confined shell never gets the key's variable"
    );
    for (style, path) in [
        (ApiStyle::Responses, "/v1/responses"),
        (ApiStyle::ChatCompletions, "/v1/chat/completions"),
    ] {
        let fake = FakeEndpoint::start(
            models(),
            vec![
                Reply::Reasoning("thinking it over", "the answer"),
                Reply::Tool {
                    name: "read_file",
                    args: json!({"path": "src/lib.rs"}),
                },
            ],
        )
        .await;
        let endpoint = Endpoint {
            name: "litellm".into(),
            dialect: WireDialect::OpenAi,
            base_url: Some(fake.base_url.clone()),
            api_key: SecretString::new(key()),
            key_env: Some(KEY_ENV.into()),
            builtin: false,
            priority: 0,
            api_style: style,
        };
        let projection = EndpointProjection::of(std::slice::from_ref(&endpoint)).unwrap();
        let mut config = meerkat_core::Config::default();
        config
            .realm
            .insert(ENDPOINT_REALM.to_string(), projection.section);
        config.self_hosted.servers = projection.servers;
        assert_eq!(
            config.self_hosted.servers["litellm"].api_style,
            style.engine()
        );
        let alias = endpoint.engine_model(CLAUDE);
        let mut entry = alias_entry(&endpoint, CLAUDE, None);
        entry.supports_reasoning = true;
        config.self_hosted.models.insert(alias.clone(), entry);
        let identity = meerkat_core::SessionLlmIdentity {
            model: alias.clone(),
            provider: Provider::SelfHosted,
            self_hosted_server_id: endpoint.self_hosted_server(),
            provider_params: None,
            auth_binding: endpoint.binding(),
        };
        let tmp = tempfile::tempdir().unwrap();
        let factory = meerkat::AgentFactory::new(tmp.path().to_path_buf());
        let client = factory
            .build_llm_client_for_identity(&config, &identity)
            .await
            .unwrap_or_else(|err| panic!("{style:?}: {err}"));
        let (text, reasoning, tools) = stream_once(&client, &alias).await;
        assert_eq!(text, "the answer", "{style:?}");
        assert_eq!(reasoning, "thinking it over", "{style:?}");
        assert!(tools.is_empty(), "{style:?}");
        let (_, _, tools) = stream_once(&client, &alias).await;
        assert_eq!(
            tools,
            [("read_file".to_string(), json!({"path": "src/lib.rs"}))],
            "{style:?}"
        );
        let turns = fake.turns();
        assert_eq!(turns.len(), 2, "{style:?}");
        for turn in &turns {
            assert_eq!(turn.path, path, "{style:?}");
            assert_eq!(turn.body["model"], CLAUDE, "{style:?}");
            assert_eq!(
                turn.header("authorization"),
                Some(format!("Bearer {}", key()).as_str()),
                "{style:?}"
            );
            assert!(!has_key(&turn.body, "thinking"), "{style:?}");
        }
    }
}

/// X2: the `api_style` setting end to end through the daemon. On the same
/// base URL (the proxy's root, as the manual asks) the default speaks the
/// Responses API and `api_style = "chat_completions"` Chat Completions; in
/// both a turn streams reasoning, calls a tool through the gate, gets its
/// result back on the wire, and answers. The projected server names the
/// style, with `/v1` added for Chat Completions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_api_style_setting_picks_the_wire_end_to_end() {
    for (style, extra, path) in [
        ("responses", &[][..], "/v1/responses"),
        (
            "chat_completions",
            &["--set", "endpoints.litellm.api_style=chat_completions"][..],
            "/v1/chat/completions",
        ),
    ] {
        let fake = FakeEndpoint::start(
            models(),
            vec![
                Reply::Tool {
                    name: "read_file",
                    args: json!({"path": "notes.txt"}),
                },
                Reply::Reasoning("looked at the notes", "the notes say hi"),
            ],
        )
        .await;
        let ws = Workspace::new();
        std::fs::write(ws.root.join("notes.txt"), "marker-7f3a\n").unwrap();
        let llm = ScriptedLlmClient::new(Vec::new());
        let mut rig = Attached::start_with(&ws.root, &llm, setup(&fake, CLAUDE, extra)).await;
        rig.saw(|m| is_model(m, "litellm", CLAUDE)).await;
        rig.send(Effect::StartTurn("read the notes".into()));
        let outcome = rig.turn_finished().await;
        assert_eq!(outcome.result.unwrap().text, "the notes say hi", "{style}");
        assert!(
            llm.recorded().is_empty(),
            "{style}: the engine's own client ran"
        );
        assert!(
            rig.seen.iter().any(|m| matches!(m, Msg::Agent(env) if matches!(
                &env.payload,
                meerkat_core::AgentEvent::ReasoningDelta { delta, .. } if delta.contains("looked")
            ))),
            "{style}: the reasoning streamed"
        );
        let turns = fake.turns();
        assert_eq!(turns.len(), 2, "{style}: {turns:#?}");
        for turn in &turns {
            assert_eq!(turn.path, path, "{style}");
            assert_eq!(turn.body["model"], CLAUDE, "{style}");
        }
        assert!(
            turns[1].body.to_string().contains("marker-7f3a"),
            "{style}: the tool's result went back: {}",
            turns[1].body
        );
        rig.quit().await;
        let (config, text) = document(&ws).await;
        let server = &config.self_hosted.servers["litellm"];
        assert!(text.contains(&format!("api_style = \"{style}\"")), "{text}");
        let expected = match style {
            "responses" => fake.base_url.clone(),
            _ => format!("{}/v1", fake.base_url),
        };
        assert_eq!(server.base_url, expected, "{style}");
    }
}

/// X2: an unknown `api_style` in a `tui.toml` is refused, naming the key
/// and the values it takes; on another dialect the endpoint is skipped
/// with a notice saying why.
#[test]
fn an_unknown_api_style_is_refused_naming_the_key() {
    let ws = Workspace::new();
    std::fs::create_dir_all(ws.root.join(".rkat")).unwrap();
    std::fs::write(
        ws.root.join(".rkat/tui.toml"),
        "[endpoints.litellm]\ndialect = \"openai\"\nbase_url = \"http://h\"\napi_style = \"completions\"\n",
    )
    .unwrap();
    let args = common::args_with(&ws.root, &[]);
    // The project file layer is parsed at start: its error names the key.
    let err = bootstrap::read_layers(&args).expect_err("an unknown api_style");
    let text = format!("{err:#}");
    assert!(
        text.contains(
            "`api_style` must be \"responses\" or \"chat_completions\", not \"completions\""
        ),
        "{text}"
    );
}
