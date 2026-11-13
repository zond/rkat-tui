//! The project `.rkat/config.toml` is judged over the bytes the factory
//! reads, not only by the guard's earlier read.
//!
//! A session build reads the realm config through `RealmConfigStore` (whose
//! `ProjectInputs::guard` checks the project hook file and shadows an
//! untrusted file's hooks) and then hands that config to
//! `AgentFactory::build_agent`, which reads the project hook file again
//! (meerkat's `FactoryAgentBuilder` does exactly these two steps). These
//! tests write the file in that interval, between the store's `get` and the
//! factory's build, with the factory configured as bootstrap configures it
//! (`with_hook_source_trust(ProjectInputs)`). That the interval is real
//! without a trust, and that an entry the user file states identically
//! survives the refusal, are the engine's own properties, tested upstream
//! (`meerkat` `hook_source_trust.rs`:
//! `without_a_trust_the_project_hooks_run_as_before`,
//! `an_identical_user_entry_runs_from_an_untrusted_project`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use clap::Parser;
use common::{MODEL, ScriptedLlmClient, Step, within};
use meerkat::{AgentBuildConfig, AgentFactory, LlmClient};
use meerkat_core::confinement::ConfinementRefusal;
use meerkat_core::{
    AgentEvent, AgentToolDispatcher, Config, ConfigStore, HookAdapterConfig, HookCapability,
    HookEntryConfig, HookFailureReason, HookId, HookPoint, ToolCallView, ToolDef,
    ToolDispatchOutcome, ToolError, ToolResult,
};
use rkat_tui::Args;
use rkat_tui::bootstrap::project_trust::ProjectInputs;
use rkat_tui::bootstrap::{RealmConfigStore, approval::trust_record, resolve_scope};
use serde_json::json;

/// A tool a `pre_tool_execution` guardrail guards; counts its dispatches.
#[derive(Default)]
struct GuardedTool(AtomicUsize);

#[async_trait]
impl AgentToolDispatcher for GuardedTool {
    fn tools(&self) -> Arc<[Arc<ToolDef>]> {
        vec![Arc::new(ToolDef::new(
            "guarded_tool",
            "A tool a PreTool hook guards",
            json!({"type": "object"}),
        ))]
        .into()
    }

    async fn dispatch(&self, call: ToolCallView<'_>) -> Result<ToolDispatchOutcome, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::new(call.id.into(), "tool completed".into(), false).into())
    }
}

/// A workspace with a separate user config root, and its project inputs.
struct Fixture {
    _dir: tempfile::TempDir,
    base: PathBuf,
    ws: PathBuf,
    home: PathBuf,
    scope: rkat_tui::bootstrap::RuntimeScope,
    record: rkat_tui::bootstrap::project_trust::SharedTrustRecord,
    inputs: ProjectInputs,
    store: RealmConfigStore,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let ws = base.join("ws");
    let home = base.join("home");
    std::fs::create_dir_all(ws.join(".rkat")).unwrap();
    std::fs::create_dir_all(home.join(".rkat")).unwrap();
    let args = Args::try_parse_from([
        "rkat-tui",
        "-w",
        ws.to_str().unwrap(),
        "--user-config-root",
        home.to_str().unwrap(),
        "--config-dir",
        base.join("cfg").to_str().unwrap(),
    ])
    .unwrap();
    let scope = resolve_scope(&args).unwrap();
    let record = Arc::new(parking_lot::Mutex::new(trust_record(&scope)));
    let inputs = ProjectInputs::new(&scope, Arc::clone(&record), false);
    let store = RealmConfigStore::new(&scope.config_path(), scope.config_state_path(), None)
        .with_inputs(inputs.clone());
    Fixture {
        _dir: dir,
        base,
        ws,
        home,
        scope,
        record,
        inputs,
        store,
    }
}

/// A guardrail whose only effect, when it runs, is to create `marker`.
fn marker_guardrail(id: &str, marker: &Path) -> HookEntryConfig {
    HookEntryConfig {
        id: HookId::new(id),
        point: HookPoint::PreToolExecution,
        capability: HookCapability::Guardrail,
        timeout_ms: Some(5_000),
        runtime: HookAdapterConfig::command(
            "/bin/sh",
            vec![
                "-c".into(),
                "printf entered > \"$1\"; cat >/dev/null; printf '{}'".into(),
                "hook-probe".into(),
                marker.display().to_string(),
            ],
            HashMap::new(),
        ),
        ..Default::default()
    }
}

/// `file` as a conventions config stating `entries` as its hooks.
fn write_hooks(file: &Path, entries: Vec<HookEntryConfig>) {
    #[derive(serde::Serialize)]
    struct Conventions {
        hooks: meerkat_core::HooksConfig,
    }
    let text = toml::to_string(&Conventions {
        hooks: meerkat_core::HooksConfig {
            entries,
            ..Default::default()
        },
    })
    .unwrap();
    std::fs::write(file, text).unwrap();
}

struct Run {
    dispatched: usize,
    refused: Vec<(HookId, HookFailureReason)>,
}

/// Builds an agent as bootstrap's factory does (with `trust` installed)
/// over `config`, and runs one turn that calls the guarded tool.
async fn build_and_run(fx: &Fixture, config: &Config, trust: &ProjectInputs) -> Run {
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "guarded-call",
            name: "guarded_tool",
            args: json!({}),
        },
        Step::Text("continued"),
    ]);
    let tool = Arc::new(GuardedTool::default());
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let mut build = AgentBuildConfig::new(MODEL);
    build.llm_client_override = Some(llm as Arc<dyn LlmClient>);
    build.external_tools = Some(Arc::clone(&tool) as Arc<dyn AgentToolDispatcher>);
    build.event_tx = Some(tx);
    let factory = AgentFactory::new(fx.base.join("sessions"))
        .session_store(Arc::new(meerkat_store::MemoryStore::new()))
        .runtime_root(fx.base.join("runtime"))
        .project_root(fx.ws.as_path())
        .context_root(fx.ws.clone())
        .user_config_root(fx.home.clone())
        .builtins(false)
        .shell(false)
        .with_hook_source_trust(Arc::new(trust.clone()));
    let mut agent = within(factory.build_agent(build, config)).await.unwrap();
    let result = within(agent.run("Attempt the guarded operation".to_string().into()))
        .await
        .unwrap();
    assert_eq!(result.text, "continued");
    let mut refused = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::HookLaunchRefused {
            hook_id, reason, ..
        } = event
        {
            refused.push((hook_id, reason));
        }
    }
    Run {
        dispatched: tool.0.load(Ordering::SeqCst),
        refused,
    }
}

fn untrusted_source(id: &str) -> (HookId, HookFailureReason) {
    (
        HookId::new(id),
        HookFailureReason::ConfinementRefused {
            refusal: ConfinementRefusal::UntrustedConfigurationSource,
        },
    )
}

/// The race the guard alone left open: the project file is trusted (no
/// hooks) when the store serves the build config, a hook is written into it
/// before the factory reads it, and the factory's question about the bytes
/// it read, answered by rkat-tui's `ProjectInputs`, refuses the hook.
#[tokio::test]
async fn a_hook_written_between_the_guard_and_the_build_never_runs() {
    let fx = fixture();
    let project = fx.ws.join(".rkat/config.toml");
    let marker = fx.base.join("planted-hook-entered");
    write_hooks(&project, vec![]);
    fx.inputs.trust_all().unwrap();
    let config = fx.store.get().await.unwrap();
    assert!(config.hooks.entries.is_empty(), "nothing to shadow");

    // The interval between the guard's read and the factory's.
    write_hooks(&project, vec![marker_guardrail("planted", &marker)]);

    let run = build_and_run(&fx, &config, &fx.inputs).await;
    assert!(!marker.exists(), "the planted hook never ran");
    assert_eq!(run.dispatched, 0, "the tool it gates is withheld");
    assert_eq!(run.refused, [untrusted_source("planted")]);
}

/// A project file untrusted when the store serves the build config keeps
/// today's behaviour: its hooks are shadowed by disabled entries, so they
/// run nothing and gate nothing, and the factory has nothing to refuse.
#[tokio::test]
async fn a_project_file_untrusted_before_the_build_stays_inert() {
    let fx = fixture();
    let project = fx.ws.join(".rkat/config.toml");
    let marker = fx.base.join("planted-hook-entered");
    write_hooks(&project, vec![marker_guardrail("planted", &marker)]);
    let config = fx.store.get().await.unwrap();
    assert_eq!(config.hooks.entries.len(), 1);
    assert!(!config.hooks.entries[0].enabled);

    let run = build_and_run(&fx, &config, &fx.inputs).await;
    assert!(!marker.exists());
    assert_eq!(run.dispatched, 1);
    assert!(run.refused.is_empty());
}

/// The trusted side of the same question, through the factory: the recorded
/// bytes run their hook, and `--trust-project` admits bytes nobody recorded
/// (a hook planted after `/trust`) for the run.
#[tokio::test]
async fn recorded_bytes_and_the_flag_run_the_project_hook() {
    let fx = fixture();
    let project = fx.ws.join(".rkat/config.toml");
    let marker = fx.base.join("recorded-hook-entered");
    write_hooks(&project, vec![marker_guardrail("fmt", &marker)]);
    fx.inputs.trust_all().unwrap();
    let config = fx.store.get().await.unwrap();
    let run = build_and_run(&fx, &config, &fx.inputs).await;
    assert!(marker.exists(), "the recorded hook ran");
    assert_eq!(run.dispatched, 1);
    assert!(run.refused.is_empty());

    let planted = fx.base.join("planted-hook-entered");
    write_hooks(&project, vec![marker_guardrail("planted", &planted)]);
    let flagged = ProjectInputs::new(&fx.scope, Arc::clone(&fx.record), true);
    let run = build_and_run(&fx, &config, &flagged).await;
    assert!(planted.exists(), "--trust-project admits it for the run");
    assert_eq!(run.dispatched, 1);
    assert!(run.refused.is_empty());
}

/// The user's own `~/.rkat/config.toml` is never judged by the record: its
/// hook runs with the trust installed and nothing recorded.
#[tokio::test]
async fn a_user_file_hook_runs_without_a_record() {
    let fx = fixture();
    let marker = fx.base.join("user-hook-entered");
    write_hooks(
        &fx.home.join(".rkat/config.toml"),
        vec![marker_guardrail("fmt", &marker)],
    );
    let config = fx.store.get().await.unwrap();
    let run = build_and_run(&fx, &config, &fx.inputs).await;
    assert!(marker.exists(), "the user's hook ran");
    assert_eq!(run.dispatched, 1);
    assert!(run.refused.is_empty());
}
