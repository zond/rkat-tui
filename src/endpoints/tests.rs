//! The endpoints' tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

/// Catalogued ids (the tests pin them: inference is catalog-exact).
const ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";
const OPENAI_MODEL: &str = "gpt-5.5";
const GEMINI_MODEL: &str = "gemini-3.1-pro-preview";

fn ep(name: &str, dialect: WireDialect, builtin: bool) -> Endpoint {
    let mut endpoint = if builtin {
        Endpoint::builtin(dialect, None, SecretString::new("k"))
    } else {
        Endpoint::configured(
            name,
            dialect,
            None,
            SecretString::new("k"),
            "KEY",
            0,
            ApiStyle::default(),
        )
    };
    endpoint.name = name.to_string();
    endpoint
}

fn r(endpoint: &str, model: &str) -> EndpointRef {
    EndpointRef::new(endpoint, model)
}

#[test]
fn dialects_come_from_the_catalog_never_from_a_prefix() {
    assert_eq!(
        WireDialect::of_catalog_model(ANTHROPIC_MODEL),
        Some(WireDialect::Anthropic)
    );
    assert_eq!(
        WireDialect::of_catalog_model(OPENAI_MODEL),
        Some(WireDialect::OpenAi)
    );
    assert_eq!(
        WireDialect::of_catalog_model(GEMINI_MODEL),
        Some(WireDialect::Gemini)
    );
    // Prefixes that used to decide no longer do.
    for unknown in [
        "claude-not-catalogued",
        "gpt-not-catalogued",
        "o9",
        "gemini-x",
    ] {
        assert_eq!(WireDialect::of_catalog_model(unknown), None, "{unknown}");
    }
}

#[test]
fn parse_model_ref_table() {
    let builtins = vec![
        ep("anthropic", WireDialect::Anthropic, true),
        ep("openai", WireDialect::OpenAi, true),
        ep("litellm", WireDialect::OpenAi, false),
        ep("vertex", WireDialect::Gemini, false),
    ];
    let two_openai = vec![
        ep("litellm", WireDialect::OpenAi, false),
        ep("proxy", WireDialect::OpenAi, false),
    ];
    let single = vec![ep("litellm", WireDialect::OpenAi, false)];
    type Case<'a> = (&'a str, &'a [Endpoint], Result<EndpointRef, &'a str>);
    let cases: Vec<Case<'_>> = vec![
        // explicit
        ("litellm:gpt-x", &builtins, Ok(r("litellm", "gpt-x"))),
        ("anthropic:gpt-x", &builtins, Ok(r("anthropic", "gpt-x"))),
        // model ids containing ':' after an explicit endpoint
        (
            "litellm:llama3:8b",
            &builtins,
            Ok(r("litellm", "llama3:8b")),
        ),
        // bare catalogued: builtin of its dialect wins over configured ones
        (OPENAI_MODEL, &builtins, Ok(r("openai", OPENAI_MODEL))),
        (
            ANTHROPIC_MODEL,
            &builtins,
            Ok(r("anthropic", ANTHROPIC_MODEL)),
        ),
        // bare catalogued: the only endpoint of the dialect
        (GEMINI_MODEL, &builtins, Ok(r("vertex", GEMINI_MODEL))),
        // bare: the only configured endpoint
        ("llama3:8b", &single, Ok(r("litellm", "llama3:8b"))),
        (ANTHROPIC_MODEL, &single, Ok(r("litellm", ANTHROPIC_MODEL))),
        // bare, unlisted, outside the catalog: no prefix guess
        (
            "gpt-not-catalogued",
            &builtins,
            Err("cannot tell which endpoint"),
        ),
        (
            "claude-not-catalogued",
            &builtins,
            Err("cannot tell which endpoint"),
        ),
        // ambiguous
        (OPENAI_MODEL, &two_openai, Err("cannot tell which endpoint")),
        ("mystery", &builtins, Err("cannot tell which endpoint")),
        ("nope:gpt-x", &two_openai, Err("unknown endpoint `nope`")),
        ("", &builtins, Err("empty")),
        ("litellm:", &builtins, Err("empty")),
        (OPENAI_MODEL, &[], Err("no LLM endpoint")),
    ];
    for (input, endpoints, expected) in cases {
        let got = parse_model_ref(input, endpoints, &ModelIndex::default(), None);
        match expected {
            Ok(want) => assert_eq!(got.as_ref(), Ok(&want), "{input}"),
            Err(needle) => {
                let err = got.expect_err(input).to_string();
                assert!(err.contains(needle), "{input}: {err}");
            }
        }
    }
    let err = parse_model_ref(OPENAI_MODEL, &two_openai, &ModelIndex::default(), None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("litellm, proxy"), "{err}");
}

#[test]
fn bare_ids_resolve_by_lookup_with_tie_breaks() {
    let mut high = ep("proxy", WireDialect::OpenAi, false);
    high.priority = 5;
    let endpoints = vec![
        ep("anthropic", WireDialect::Anthropic, true),
        ep("openai", WireDialect::OpenAi, true),
        ep("litellm", WireDialect::OpenAi, false),
        high,
    ];
    let mut index = ModelIndex::default();
    index.insert(
        "litellm",
        ["only-on-litellm", OPENAI_MODEL, "shared", ANTHROPIC_MODEL],
    );
    index.insert("openai", [OPENAI_MODEL]);
    index.insert("proxy", ["shared", OPENAI_MODEL, ANTHROPIC_MODEL]);
    index.insert("anthropic", [ANTHROPIC_MODEL]);
    type Case<'a> = (&'a str, Option<&'a str>, Result<EndpointRef, &'a str>);
    let cases: Vec<Case<'_>> = vec![
        // unique: listed by one endpoint, whatever the catalog says
        ("only-on-litellm", None, Ok(r("litellm", "only-on-litellm"))),
        // several: the current endpoint first
        (
            OPENAI_MODEL,
            Some("litellm"),
            Ok(r("litellm", OPENAI_MODEL)),
        ),
        (
            ANTHROPIC_MODEL,
            Some("proxy"),
            Ok(r("proxy", ANTHROPIC_MODEL)),
        ),
        // then the builtin of the catalogued id's dialect
        (
            OPENAI_MODEL,
            Some("anthropic"),
            Ok(r("openai", OPENAI_MODEL)),
        ),
        (ANTHROPIC_MODEL, None, Ok(r("anthropic", ANTHROPIC_MODEL))),
        // then the highest priority
        ("shared", None, Ok(r("proxy", "shared"))),
        // unlisted catalogued id → its provider's built-in endpoint
        ("gpt-5.4", None, Ok(r("openai", "gpt-5.4"))),
        // unlisted, outside the catalog, several endpoints → error
        (
            "gpt-not-catalogued",
            None,
            Err("cannot tell which endpoint"),
        ),
        ("mystery", None, Err("cannot tell which endpoint")),
        // explicit override
        ("litellm:gpt-6", None, Ok(r("litellm", "gpt-6"))),
    ];
    for (input, current, expected) in cases {
        let got = parse_model_ref(input, &endpoints, &index, current);
        match expected {
            Ok(want) => assert_eq!(got.as_ref(), Ok(&want), "{input}"),
            Err(needle) => {
                let err = got.expect_err(input).to_string();
                assert!(err.contains(needle), "{input}: {err}");
            }
        }
    }
    // Equal priorities: the first configured wins.
    let tied = vec![
        ep("a", WireDialect::OpenAi, false),
        ep("b", WireDialect::OpenAi, false),
    ];
    let mut index = ModelIndex::default();
    index.insert("b", ["m"]);
    index.insert("a", ["m"]);
    assert_eq!(parse_model_ref("m", &tied, &index, None), Ok(r("a", "m")));
    // Unlisted, unknown, single endpoint → that endpoint.
    let single = vec![ep("litellm", WireDialect::OpenAi, false)];
    assert_eq!(
        parse_model_ref("mystery", &single, &ModelIndex::default(), None),
        Ok(r("litellm", "mystery"))
    );
    assert!(needs_lookup(OPENAI_MODEL, &endpoints));
    assert!(!needs_lookup("litellm:gpt-x", &endpoints));
    assert!(!needs_lookup(OPENAI_MODEL, &single));
}

#[test]
fn endpoint_ref_display() {
    assert_eq!(r("litellm", "gpt-x").to_string(), "litellm:gpt-x");
}

#[test]
fn secret_never_debugs() {
    let mut e = ep("x", WireDialect::OpenAi, false);
    e.api_key = SecretString::new("sk-very-secret");
    assert!(!format!("{e:?}").contains("sk-very-secret"));
    assert!(!format!("{:?}", e.credentials()).contains("sk-very-secret"));
}

fn layered(section: EndpointSection, origin: RuleOrigin) -> LayeredEndpoint {
    LayeredEndpoint { section, origin }
}

fn section(dialect: Option<&str>, key_env: Option<&str>, key: Option<&str>) -> EndpointSection {
    EndpointSection {
        dialect: dialect.map(str::to_string),
        base_url: Some("http://localhost:4000".to_string()),
        api_key_env: key_env.map(str::to_string),
        api_key: key.map(str::to_string),
        priority: None,
        api_style: None,
    }
}

#[test]
fn chat_completions_gets_v1_and_responses_keeps_the_url() {
    for (url, chat) in [
        ("http://h:4000", "http://h:4000/v1"),
        ("http://h:4000/", "http://h:4000/v1"),
        ("http://h:4000/v1", "http://h:4000/v1"),
        ("http://h:4000/v1/", "http://h:4000/v1"),
        ("https://h/openai", "https://h/openai/v1"),
    ] {
        assert_eq!(server_base_url(url, ApiStyle::ChatCompletions), chat);
        assert_eq!(server_base_url(url, ApiStyle::Responses), url);
    }
}

#[test]
fn api_style_is_parsed_strictly_and_only_for_the_openai_dialect() {
    assert_eq!(ApiStyle::parse("responses"), Ok(ApiStyle::Responses));
    assert_eq!(
        ApiStyle::parse("chat_completions"),
        Ok(ApiStyle::ChatCompletions)
    );
    for bad in ["Responses", "chat", "chat-completions", ""] {
        let err = ApiStyle::parse(bad).unwrap_err().to_string();
        assert!(err.starts_with("`api_style` must be"), "{err}");
    }
    let env = |_: &str| Some("k".to_string());
    let mut chat = section(Some("openai"), Some("K"), None);
    chat.api_style = Some(ApiStyle::ChatCompletions);
    let endpoint = configured_endpoint("p", &layered(chat, RuleOrigin::Global), &env).unwrap();
    assert_eq!(endpoint.api_style(), ApiStyle::ChatCompletions);
    let mut gateway = section(Some("anthropic"), Some("K"), None);
    gateway.api_style = Some(ApiStyle::Responses);
    assert_eq!(
        configured_endpoint("g", &layered(gateway, RuleOrigin::Global), &env).unwrap_err(),
        EndpointSkip::ApiStyleDialect(WireDialect::Anthropic)
    );
    let plain = configured_endpoint(
        "q",
        &layered(section(Some("openai"), Some("K"), None), RuleOrigin::Global),
        &env,
    )
    .unwrap();
    assert_eq!(plain.api_style(), ApiStyle::Responses);
}

#[test]
fn discover_validates_configured_endpoints() {
    let config = Config::default();
    let configured: BTreeMap<String, LayeredEndpoint> = [
        (
            "litellm",
            layered(
                section(Some("openai"), Some("LITELLM_KEY"), None),
                RuleOrigin::Realm,
            ),
        ),
        (
            "baddialect",
            layered(
                section(Some("cohere"), Some("LITELLM_KEY"), None),
                RuleOrigin::Realm,
            ),
        ),
        (
            "nodialect",
            layered(section(None, Some("LITELLM_KEY"), None), RuleOrigin::Realm),
        ),
        (
            "nokey",
            layered(section(Some("openai"), None, None), RuleOrigin::Realm),
        ),
        (
            "unsetenv",
            layered(
                section(Some("openai"), Some("UNSET_VAR"), None),
                RuleOrigin::Realm,
            ),
        ),
        (
            "openai",
            layered(
                section(Some("openai"), Some("LITELLM_KEY"), None),
                RuleOrigin::Realm,
            ),
        ),
        (
            "projenv",
            layered(
                section(Some("openai"), Some("LITELLM_KEY"), None),
                RuleOrigin::Project,
            ),
        ),
        (
            "bad.name",
            layered(
                section(Some("openai"), Some("LITELLM_KEY"), None),
                RuleOrigin::Realm,
            ),
        ),
    ]
    .into_iter()
    .map(|(n, l)| (n.to_string(), l))
    .collect();
    let (registry, notices) = EndpointRegistry::discover_with(&config, None, &configured, |var| {
        (var == "LITELLM_KEY").then(|| "sk-litellm".to_string())
    })
    .unwrap();
    let litellm = registry.get("litellm").expect("litellm");
    assert_eq!(litellm.dialect, WireDialect::OpenAi);
    assert_eq!(litellm.api_key.expose(), "sk-litellm");
    assert!(!litellm.is_builtin());
    for skipped in [
        "baddialect",
        "nodialect",
        "nokey",
        "unsetenv",
        "projenv",
        "bad.name",
    ] {
        assert!(registry.get(skipped).is_none(), "{skipped}");
        assert!(
            notices
                .iter()
                .any(|n| matches!(n, Notice::EndpointSkipped { name, .. } if name == skipped)),
            "{skipped}: {notices:?}"
        );
    }
    // The configured `openai` clashes with the built-in name.
    assert!(
        registry
            .iter()
            .all(|e| e.name != "openai" || e.is_builtin())
    );
    assert!(
        notices
            .iter()
            .any(|n| matches!(n, Notice::EndpointSkipped { name, .. } if name == "openai"))
    );
    let text: Vec<String> = notices.iter().map(ToString::to_string).collect();
    assert!(text.iter().all(|t| !t.contains("sk-litellm")));
}

#[test]
fn an_inline_api_key_is_a_typed_startup_error_naming_api_key_env() {
    let configured: BTreeMap<String, LayeredEndpoint> = [
        (
            "litellm".to_string(),
            layered(
                section(Some("openai"), Some("LITELLM_KEY"), None),
                RuleOrigin::Global,
            ),
        ),
        (
            "plain".to_string(),
            layered(
                section(Some("anthropic"), None, Some("sk-plain-secret")),
                RuleOrigin::Global,
            ),
        ),
    ]
    .into_iter()
    .collect();
    let err = EndpointRegistry::discover_with(&Config::default(), None, &configured, |_| {
        Some("sk-litellm".to_string())
    })
    .unwrap_err();
    assert!(
        matches!(&err, EndpointError::InlineApiKey { name } if name == "plain"),
        "{err}"
    );
    let text = err.to_string();
    assert!(text.contains("api_key_env"), "{text}");
    assert!(text.contains("[endpoints.plain]"), "{text}");
    assert!(!text.contains("sk-plain-secret"), "{text}");
    // A start-up error: exit 2.
    assert_eq!(crate::TuiError::from(err).exit_code(), 2);
}

fn plan_startup_t(
    request: StartupRequest<'_>,
    endpoints: &[Endpoint],
) -> Result<StartupPlan, ModelRefError> {
    plan_startup(request, endpoints, &ModelIndex::default())
}

#[test]
fn startup_plans() {
    let both = vec![
        ep("anthropic", WireDialect::Anthropic, true),
        ep("litellm", WireDialect::OpenAi, false),
    ];
    let only_litellm = vec![ep("litellm", WireDialect::OpenAi, false)];
    let only_openai = vec![ep("openai", WireDialect::OpenAi, true)];
    let req = |model, endpoint, config_model| StartupRequest {
        model,
        endpoint,
        config_model,
    };
    let resolved = |e: &str, m: &str| Ok(StartupPlan::Resolved(r(e, m)));
    assert_eq!(
        plan_startup_t(req(Some("litellm:gpt-x"), None, ""), &both),
        resolved("litellm", "gpt-x")
    );
    assert_eq!(
        plan_startup_t(req(Some("gpt-x"), Some("litellm"), ""), &both),
        resolved("litellm", "gpt-x")
    );
    assert_eq!(
        plan_startup_t(req(Some(ANTHROPIC_MODEL), None, ""), &both),
        resolved("anthropic", ANTHROPIC_MODEL)
    );
    assert_eq!(
        plan_startup_t(req(None, Some("litellm"), ANTHROPIC_MODEL), &both),
        Ok(StartupPlan::FirstListed("litellm".into()))
    );
    assert_eq!(
        plan_startup_t(req(None, None, ANTHROPIC_MODEL), &both),
        resolved("anthropic", ANTHROPIC_MODEL)
    );
    // Config model of a dialect nobody serves, one endpoint: list it.
    assert_eq!(
        plan_startup_t(req(None, None, ANTHROPIC_MODEL), &only_litellm),
        Ok(StartupPlan::FirstListed("litellm".into()))
    );
    // ... or use the catalog default of a built-in.
    let Ok(StartupPlan::Resolved(choice)) =
        plan_startup_t(req(None, None, ANTHROPIC_MODEL), &only_openai)
    else {
        panic!("expected a resolved model");
    };
    assert_eq!(choice.endpoint, "openai");
    assert_eq!(
        Some(choice.model.as_str()),
        meerkat_models::default_model(Provider::OpenAI)
    );
    let err = plan_startup_t(req(None, Some("nope"), ""), &both).unwrap_err();
    assert!(err.to_string().contains("anthropic, litellm"), "{err}");
    let err = plan_startup_t(
        req(None, None, "mystery"),
        &[
            ep("litellm", WireDialect::OpenAi, false),
            ep("proxy", WireDialect::OpenAi, false),
        ],
    )
    .unwrap_err();
    assert!(matches!(err, ModelRefError::NoModel { .. }), "{err}");
    assert_eq!(
        plan_startup_t(req(None, None, ""), &[]),
        Err(ModelRefError::NoEndpoints)
    );
}

#[test]
fn a_sessions_endpoint_is_derived_from_its_binding_and_provider() {
    let registry = EndpointRegistry::from_endpoints(vec![
        ep("anthropic", WireDialect::Anthropic, true),
        ep("litellm", WireDialect::OpenAi, false),
    ]);
    let litellm = registry.get("litellm").unwrap().binding();
    assert_eq!(
        endpoint_of(Provider::OpenAI, litellm.as_ref(), &registry),
        Some("litellm".to_string())
    );
    assert_eq!(
        endpoint_of(Provider::Anthropic, None, &registry),
        Some("anthropic".to_string())
    );
    // No built-in openai endpoint in this process, and a binding of
    // another realm names no TUI endpoint.
    assert_eq!(endpoint_of(Provider::OpenAI, None, &registry), None);
    let foreign = AuthBindingRef {
        realm: RealmId::parse("elsewhere").unwrap(),
        binding: BindingId::parse("litellm").unwrap(),
        profile: None,
        origin: BindingOrigin::Configured,
    };
    assert_eq!(
        endpoint_of(Provider::OpenAI, Some(&foreign), &registry),
        None
    );
    // A removed endpoint is no endpoint.
    let gone = ep("gone", WireDialect::OpenAi, false).binding();
    assert_eq!(
        endpoint_of(Provider::OpenAI, gone.as_ref(), &registry),
        None
    );
}

fn configured(name: &str, dialect: WireDialect, key_env: &str) -> Endpoint {
    Endpoint::configured(
        name,
        dialect,
        Some(format!("http://127.0.0.1:4000/{name}")),
        SecretString::new("sk-inline"),
        key_env,
        0,
        ApiStyle::default(),
    )
}

#[test]
fn configured_endpoints_become_bindings_off_every_chain() {
    let endpoints = vec![
        ep("openai", WireDialect::OpenAi, true),
        configured("litellm", WireDialect::OpenAi, "LITELLM_KEY"),
        configured("gateway", WireDialect::Anthropic, "GATEWAY_KEY"),
        configured("vertex", WireDialect::Gemini, "VERTEX_KEY"),
    ];
    let mut config = Config::default();
    let before = meerkat_core::connection::resolve_auth_binding_or_default_for_provider(
        &config,
        Provider::OpenAI,
        None,
        None,
        true,
    )
    .unwrap();
    config.realm.insert(
        ENDPOINT_REALM.to_string(),
        endpoint_section(&endpoints).unwrap(),
    );
    assert!(
        endpoints[0].binding().is_none(),
        "built-ins resolve by default"
    );
    for endpoint in &endpoints[1..] {
        let binding = endpoint.binding().unwrap();
        let target =
            meerkat_core::connection::resolve_explicit_auth_binding_target(&config, &binding)
                .unwrap();
        // An OpenAI-dialect proxy is the engine's self-hosted server.
        assert_eq!(
            target.backend.provider,
            endpoint.engine_provider(),
            "{}",
            endpoint.name
        );
        assert_eq!(
            target.backend.server,
            endpoint.self_hosted_server(),
            "{}",
            endpoint.name
        );
        assert_eq!(
            target.backend.base_url, endpoint.base_url,
            "{}",
            endpoint.name
        );
        let expected = CredentialSourceSpec::Env {
            env: endpoint.key_env().unwrap().to_string(),
            fallback: Vec::new(),
        };
        assert!(target.auth_profile.source == expected, "{}", endpoint.name);
    }
    // The built-in provider's default resolution is untouched.
    let after = meerkat_core::connection::resolve_auth_binding_or_default_for_provider(
        &config,
        Provider::OpenAI,
        None,
        None,
        true,
    )
    .unwrap();
    assert_eq!(before.auth_binding, after.auth_binding);
    assert_eq!(before.backend.base_url, after.backend.base_url);
    // Only built-ins: nothing to project.
    assert!(
        endpoint_section(&endpoints[..1])
            .unwrap()
            .binding
            .is_empty()
    );
}

/// The config runtime of the document at `path`, its sidecar beside it.
fn config_runtime(path: &std::path::Path) -> ConfigRuntime {
    ConfigRuntime::new(
        std::sync::Arc::new(meerkat_core::FileConfigStore::new(
            path.to_path_buf(),
            meerkat_models::canonical(),
        )),
        path.with_file_name("config_state.json"),
    )
}

/// A writer that commits between the read and the write makes the
/// write conflict on the generation; the patch is rebuilt from the new
/// document and written then.
#[tokio::test]
async fn a_write_that_lost_the_generation_race_is_rebuilt_and_retried() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    let runtime = config_runtime(&path);
    let sidecar = path.with_file_name("config_state.json");
    let reads = std::sync::atomic::AtomicU32::new(0);
    let wrote = patch_document(&runtime, |persisted| {
        if reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            // Another surface commits a write of its own meanwhile.
            std::fs::write(&path, "[models.\"theirs\"]\nprovider = \"openai\"\n").unwrap();
            std::fs::write(&sidecar, r#"{"generation": 7}"#).unwrap();
        }
        assert_eq!(
            persisted.models.custom.contains_key("theirs"),
            reads.load(std::sync::atomic::Ordering::SeqCst) > 1
        );
        Ok(Some(ConfigDelta(serde_json::json!({
            "models": { "ours": { "provider": "anthropic" } }
        }))))
    })
    .await
    .unwrap();
    assert!(wrote);
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2);
    let snapshot = runtime.get().await.unwrap();
    assert_eq!(snapshot.generation, 8);
    assert!(snapshot.config.models.custom.contains_key("theirs"));
    assert!(snapshot.config.models.custom.contains_key("ours"));
}

/// rkat-tui's write waits for the writer lock another surface holds.
#[tokio::test]
async fn a_write_waits_for_the_writer_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    let runtime = config_runtime(&path);
    let held = meerkat_core::config_runtime::ConfigWriterLock::acquire(
        &path.with_file_name("config_state.lock"),
    )
    .await
    .unwrap();
    let proxy = configured("litellm", WireDialect::OpenAi, "LITELLM_KEY");
    let write = project_endpoints(&runtime, std::slice::from_ref(&proxy));
    tokio::pin!(write);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), &mut write)
            .await
            .is_err(),
        "the write went past a held writer lock"
    );
    assert!(!path.exists());
    drop(held);
    assert!(write.await.unwrap());
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("LITELLM_KEY")
    );
}

#[tokio::test]
async fn endpoints_are_projected_into_the_realm_document_once() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    let store = config_runtime(&path);
    let endpoints = vec![
        ep("anthropic", WireDialect::Anthropic, true),
        configured("litellm", WireDialect::OpenAi, "LITELLM_KEY"),
    ];
    assert!(project_endpoints(&store, &endpoints).await.unwrap());
    // Unchanged: nothing is written again.
    assert!(!project_endpoints(&store, &endpoints).await.unwrap());
    // What another surface loading the realm document reads resolves
    // the session's binding.
    let reread = meerkat_core::FileConfigStore::new(path.clone(), meerkat_models::canonical());
    let reread = meerkat_core::ConfigStore::get(&reread).await.unwrap();
    let binding = endpoints[1].binding().unwrap();
    let target =
        meerkat_core::connection::resolve_explicit_auth_binding_target(&reread, &binding).unwrap();
    assert_eq!(target.backend.base_url, endpoints[1].base_url);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("LITELLM_KEY"), "{text}");
    assert!(!text.contains("sk-inline"), "{text}");
    // The document records the variable's name only: no secret, and no
    // inline-secret source.
    assert!(!text.contains("secret"), "{text}");
    // (A configured endpoint without a variable cannot be made: the type
    // holds the variable's name.)
    let gateway = vec![configured("gateway", WireDialect::Anthropic, "GATEWAY_KEY")];
    assert!(project_endpoints(&store, &gateway).await.unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("GATEWAY_KEY"), "{text}");
    assert!(!text.contains("sk-inline"), "{text}");
    // Earlier endpoints stay, so their sessions keep resolving.
    let reread = store.get().await.unwrap();
    // Each write was one generation of the realm's config runtime.
    assert_eq!(reread.generation, 2);
    assert!(
        reread.config.realm[ENDPOINT_REALM]
            .binding
            .contains_key("litellm")
    );
    // One section for every configured endpoint.
    assert_eq!(
        reread.config.realm.len(),
        1,
        "{:?}",
        reread.config.realm.keys()
    );
    assert!(
        reread.config.realm[ENDPOINT_REALM]
            .binding
            .contains_key("gateway")
    );
}

/// An OpenAI-dialect proxy's models are self-hosted aliases with the
/// listing's facts: a catalogued id is no collision (no `[models]`
/// entry), the first alias becomes the default the engine needs once
/// there are two, a later listing refreshes an alias, and none keeps
/// it as it was; the endpoint's server is projected with its
/// connection.
#[tokio::test]
async fn a_proxy_s_models_become_aliases_with_its_facts() {
    let tmp = tempfile::tempdir().unwrap();
    let store = config_runtime(&tmp.path().join("config.toml"));
    let proxy = configured("litellm", WireDialect::OpenAi, "LITELLM_KEY");
    assert!(
        project_endpoints(&store, std::slice::from_ref(&proxy))
            .await
            .unwrap()
    );
    let listed = |id: &str, window: u64| ModelEntry {
        id: id.into(),
        display_name: None,
        source: crate::models::ModelSource::Provider,
        context_window: Some(window),
        max_output_tokens: Some(4_096),
        capabilities: crate::models::ModelCapabilities {
            reasoning: Some(true),
            vision: Some(true),
            function_calling: Some(true),
        },
    };
    let opus = listed(ANTHROPIC_MODEL, 50_000);
    let local = listed("llama3:8b", 8_192);
    assert!(
        ensure_aliases(
            &store,
            &proxy,
            &[(ANTHROPIC_MODEL, Some(&opus)), ("llama3:8b", Some(&local))]
        )
        .await
        .unwrap()
    );
    let config = store.get().await.unwrap().config;
    assert!(config.models.custom.is_empty());
    assert_eq!(
        config.self_hosted.servers["litellm"].base_url,
        "http://127.0.0.1:4000/litellm"
    );
    let alias = format!("litellm:{ANTHROPIC_MODEL}");
    let entry = &config.self_hosted.models[&alias];
    assert_eq!(entry.server, "litellm");
    assert_eq!(entry.remote_model, ANTHROPIC_MODEL);
    assert_eq!(entry.context_window, Some(50_000));
    assert!(entry.supports_reasoning && entry.supports_thinking && entry.vision);
    assert!(entry.supports_temperature);
    assert_eq!(
        config.self_hosted.default_model.as_deref(),
        Some(alias.as_str())
    );
    let registry = config.model_registry(meerkat_models::canonical()).unwrap();
    assert_eq!(
        registry_context_window(&config, Provider::SelfHosted, "litellm:llama3:8b"),
        Some(8_192)
    );
    assert_eq!(
        registry
            .entry_for_provider(Provider::SelfHosted, &alias)
            .map(|e| e.id.clone()),
        Some(alias.clone())
    );
    // No listing now: the alias stays as it was; the same listing
    // writes nothing; a new window refreshes it.
    assert!(
        !ensure_aliases(&store, &proxy, &[(ANTHROPIC_MODEL, None)])
            .await
            .unwrap()
    );
    assert!(
        !ensure_aliases(&store, &proxy, &[(ANTHROPIC_MODEL, Some(&opus))])
            .await
            .unwrap()
    );
    let bigger = listed(ANTHROPIC_MODEL, 60_000);
    assert!(
        ensure_endpoint_model(&store, &proxy, ANTHROPIC_MODEL, Some(&bigger))
            .await
            .unwrap()
    );
    let config = store.get().await.unwrap().config;
    assert_eq!(
        config.self_hosted.models[&alias].context_window,
        Some(60_000)
    );
    // A built-in or another dialect's endpoint writes no alias.
    let gateway = configured("gateway", WireDialect::Anthropic, "GATEWAY_KEY");
    assert!(
        !ensure_aliases(&store, &gateway, &[("x", None)])
            .await
            .unwrap()
    );
}

#[test]
fn an_engine_alias_is_the_model_on_its_endpoint() {
    assert_eq!(
        from_engine("litellm:llama3:8b", "self_hosted"),
        ("llama3:8b".to_string(), "openai".to_string())
    );
    assert_eq!(
        from_engine(ANTHROPIC_MODEL, "anthropic"),
        (ANTHROPIC_MODEL.to_string(), "anthropic".to_string())
    );
    let proxy = configured("litellm", WireDialect::OpenAi, "K");
    assert_eq!(proxy.engine_model("gpt-5.5"), "litellm:gpt-5.5");
    assert_eq!(proxy.engine_provider(), Provider::SelfHosted);
    let gateway = configured("gateway", WireDialect::Anthropic, "K");
    assert_eq!(gateway.engine_model(ANTHROPIC_MODEL), ANTHROPIC_MODEL);
    assert_eq!(gateway.engine_provider(), Provider::Anthropic);
    assert_eq!(gateway.self_hosted_server(), None);
    let builtin = ep("openai", WireDialect::OpenAi, true);
    assert!(!builtin.is_self_hosted());
}

#[tokio::test]
async fn a_configured_endpoint_builds_its_dialect_client() {
    let tmp = tempfile::tempdir().unwrap();
    let factory = meerkat::AgentFactory::new(tmp.path().to_path_buf());
    for endpoint in [
        // `PATH` is set wherever tests run; its value only has to be
        // a non-empty key for the client to build.
        configured("litellm", WireDialect::OpenAi, "PATH"),
        configured("gateway", WireDialect::Anthropic, "PATH"),
        configured("vertex", WireDialect::Gemini, "PATH"),
    ] {
        let mut config = Config::default();
        let projection = EndpointProjection::of(std::slice::from_ref(&endpoint)).unwrap();
        config
            .realm
            .insert(ENDPOINT_REALM.to_string(), projection.section);
        config.self_hosted.servers = projection.servers;
        // An OpenAI-dialect proxy runs any model as its self-hosted
        // alias; the others run catalogued models.
        let model = if endpoint.is_self_hosted() {
            let model = "served-only-here";
            config.self_hosted.models.insert(
                endpoint.engine_model(model),
                alias_entry(&endpoint, model, None),
            );
            model
        } else if endpoint.dialect == WireDialect::Anthropic {
            ANTHROPIC_MODEL
        } else {
            "gemini-3.5-flash"
        };
        let identity = meerkat_core::SessionLlmIdentity {
            model: endpoint.engine_model(model),
            provider: endpoint.engine_provider(),
            self_hosted_server_id: endpoint.self_hosted_server(),
            provider_params: None,
            auth_binding: endpoint.binding(),
        };
        let client = factory
            .build_llm_client_for_identity(&config, &identity)
            .await
            .unwrap_or_else(|err| panic!("{}: {err}", endpoint.name));
        assert_eq!(
            client.provider(),
            endpoint.engine_provider(),
            "{}",
            endpoint.name
        );
    }
}

/// Only a self-hosted endpoint runs a model the catalog does not know:
/// on a gateway or a built-in endpoint such a model is refused, naming
/// the openai-dialect way out; a catalogued one, or one the realm
/// config's own `[models.<id>]` profiles, passes.
#[tokio::test]
async fn an_uncatalogued_model_runs_only_on_a_self_hosted_endpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    let store = config_runtime(&path);
    let gateway = configured("gateway", WireDialect::Anthropic, "GATEWAY_KEY");
    let builtin = ep("openai", WireDialect::OpenAi, true);
    let proxy = configured("litellm", WireDialect::OpenAi, "LITELLM_KEY");
    assert!(
        project_endpoints(&store, std::slice::from_ref(&proxy))
            .await
            .unwrap()
    );
    let err = ensure_endpoint_model(&store, &gateway, "llama3:8b", None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, EndpointError::Uncatalogued { model, dialect: WireDialect::Anthropic, .. } if model == "llama3:8b"),
        "{err}"
    );
    let text = err.to_string();
    assert!(text.contains("endpoint `gateway`"), "{text}");
    assert!(text.contains("`openai`-dialect endpoint"), "{text}");
    let err = ensure_endpoint_model(&store, &builtin, "gpt-not-yet", None)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("the built-in endpoint `openai`"),
        "{err}"
    );
    // A catalogued model of the dialect runs; nothing is written.
    assert!(
        !ensure_endpoint_model(&store, &gateway, ANTHROPIC_MODEL, None)
            .await
            .unwrap()
    );
    // Another dialect's catalogued model is not this endpoint's.
    assert!(matches!(
        ensure_endpoint_model(&store, &gateway, OPENAI_MODEL, None).await,
        Err(EndpointError::Uncatalogued { .. })
    ));
    // The self-hosted endpoint runs it as its alias.
    assert!(
        ensure_endpoint_model(&store, &proxy, "llama3:8b", None)
            .await
            .unwrap()
    );
    // The user's own profile counts.
    std::fs::write(&path, "[models.\"llama3:8b\"]\nprovider = \"anthropic\"\n").unwrap();
    assert!(
        !ensure_endpoint_model(&store, &gateway, "llama3:8b", None)
            .await
            .unwrap()
    );
}
