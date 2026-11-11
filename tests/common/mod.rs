//! Shared test support: a temporary workspace, a scripted LLM client that
//! records every request, and the command lines of a test realm. The
//! daemon rig over them is `attach.rs` beside this file.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use clap::Parser;
use futures::StreamExt;
use meerkat::{LlmClient, LlmDoneOutcome, LlmError, LlmEvent, LlmRequest};
use meerkat_core::{Message, Provider, ProviderTag, StopReason, TurnUsage, Usage};
use parking_lot::Mutex;
use rkat_tui::Args;
use rkat_tui::models::{EndpointListing, ModelLister};
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc};

/// Model every test session runs on.
pub const MODEL: &str = "claude-sonnet-4-6";

/// Upper bound on any await that could hang on a wiring bug.
pub const TIMEOUT: Duration = Duration::from_secs(30);

pub async fn within<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .expect("timed out")
}

/// One scripted LLM response.
#[derive(Clone)]
pub enum Step {
    /// Text, then `Done(EndTurn)`.
    Text(&'static str),
    /// Text reporting `input` input tokens, then `Done(EndTurn)`.
    TextUsage(&'static str, u64),
    /// A single tool call, then `Done(ToolUse)`.
    Tool {
        id: &'static str,
        name: &'static str,
        args: Value,
    },
    /// Several tool calls `(id, name, args)`, then `Done(ToolUse)`.
    Tools(Vec<(&'static str, &'static str, Value)>),
    /// A 400 rejecting the tool definitions.
    RejectTools,
    /// A transport failure.
    StreamError,
    /// A response that never arrives.
    Hang,
    /// The inner step, once the gate is notified.
    Gated(Arc<Notify>, Box<Step>),
    /// The step this function makes of the request's messages (e.g. a call
    /// naming an id an earlier tool result returned).
    Dynamic(Arc<dyn Fn(&[Message]) -> Step + Send + Sync>),
}

impl Step {
    fn into_parts(self) -> (Option<Arc<Notify>>, Step) {
        match self {
            Self::Gated(gate, inner) => (Some(gate), *inner),
            other => (None, other),
        }
    }

    fn events(self, model: &str) -> Vec<Result<LlmEvent, LlmError>> {
        let input_tokens = match self {
            Self::TextUsage(_, input) => input,
            _ => 10,
        };
        let usage = Ok(LlmEvent::UsageUpdate {
            usage: TurnUsage::host_declared(
                Provider::Anthropic,
                model,
                Usage {
                    input_tokens,
                    output_tokens: 5,
                    ..Usage::default()
                },
            ),
        });
        let done = |stop_reason| {
            Ok(LlmEvent::Done {
                outcome: LlmDoneOutcome::success(stop_reason),
            })
        };
        match self {
            Self::Text(text) | Self::TextUsage(text, _) => vec![
                Ok(LlmEvent::TextDelta {
                    delta: text.to_string(),
                    meta: None,
                }),
                usage,
                done(StopReason::EndTurn),
            ],
            Self::Tool { id, name, args } => Self::Tools(vec![(id, name, args)]).events(model),
            Self::Tools(calls) => {
                let mut events: Vec<_> = calls
                    .into_iter()
                    .map(|(id, name, args)| {
                        Ok(LlmEvent::ToolCallComplete {
                            id: id.to_string(),
                            name: name.to_string(),
                            args,
                            meta: None,
                        })
                    })
                    .collect();
                events.push(usage);
                events.push(done(StopReason::ToolUse));
                events
            }
            Self::RejectTools => vec![Err(LlmError::InvalidRequest {
                message: "tools are not supported by this model".to_string(),
            })],
            Self::StreamError => vec![Err(LlmError::ServerOverloaded)],
            Self::Hang => Vec::new(),
            Self::Gated(_, inner) => inner.events(model),
            Self::Dynamic(_) => panic!("a dynamic step is resolved before it streams"),
        }
    }
}

/// One recorded request.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub model: String,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub provider_params: Option<ProviderTag>,
    pub tools: Vec<String>,
    pub schemas: Vec<Value>,
    /// The request's tool definitions in full.
    pub defs: Vec<Arc<meerkat_core::ToolDef>>,
    pub messages: Vec<Message>,
}

/// The family a tool definition is counted under in [`Recorded::tool_bytes`].
pub fn tool_family(def: &meerkat_core::ToolDef) -> &'static str {
    use meerkat_core::ToolSourceKind;
    if matches!(
        def.name.as_str(),
        "tool_catalog_search" | "tool_catalog_load"
    ) {
        return "catalog";
    }
    let Some(provenance) = &def.provenance else {
        return "other";
    };
    match provenance.kind {
        ToolSourceKind::Callback => match provenance.source_id.as_str() {
            rkat_tui::tools::FILE_TOOLS_SOURCE => "files",
            rkat_tui::memory::NOTES_SOURCE => "notes",
            rkat_tui::tools::WATCH_TOOLS_SOURCE => "watch",
            _ => "callback",
        },
        ToolSourceKind::Builtin | ToolSourceKind::Shell => "builtin",
        ToolSourceKind::Memory => "memory",
        ToolSourceKind::Comms => "comms",
        ToolSourceKind::Schedule => "schedule",
        ToolSourceKind::WorkGraph => "workgraph",
        ToolSourceKind::Mob => "mob",
        ToolSourceKind::Mcp => "mcp",
        ToolSourceKind::RustBundle => "bundle",
    }
}

/// The bytes one tool definition costs a request: its name, description and
/// input schema as JSON (what a provider is sent for it).
pub fn tool_def_bytes(def: &meerkat_core::ToolDef) -> usize {
    serde_json::json!({
        "name": def.name.as_str(),
        "description": def.description,
        "input_schema": def.input_schema,
    })
    .to_string()
    .len()
}

impl Recorded {
    /// The tool-definition bytes of this request by [`tool_family`].
    pub fn tool_bytes(&self) -> std::collections::BTreeMap<&'static str, usize> {
        let mut out = std::collections::BTreeMap::new();
        for def in &self.defs {
            *out.entry(tool_family(def)).or_insert(0) += tool_def_bytes(def);
        }
        out
    }

    /// The names of this request's tools of `family`.
    pub fn tools_of(&self, family: &str) -> Vec<String> {
        self.defs
            .iter()
            .filter(|def| tool_family(def) == family)
            .map(|def| def.name.to_string())
            .collect()
    }

    /// The tool-definition bytes of this request in all.
    pub fn total_tool_bytes(&self) -> usize {
        self.defs.iter().map(|def| tool_def_bytes(def)).sum()
    }

    /// Concatenated system text.
    pub fn system(&self) -> String {
        self.messages
            .iter()
            .filter_map(|m| match m {
                Message::System(s) => Some(s.content.clone()),
                _ => None,
            })
            .collect()
    }

    /// Concatenated user text.
    pub fn user(&self) -> String {
        user_texts(&self.messages).concat()
    }
}

/// Returns one scripted response per call and records every request.
pub struct ScriptedLlmClient {
    script: Vec<Step>,
    next: AtomicUsize,
    requests: Mutex<Vec<Recorded>>,
    calls: mpsc::UnboundedSender<usize>,
    calls_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<usize>>,
}

impl ScriptedLlmClient {
    /// A client replaying `script` (then answering "script exhausted").
    pub fn new(script: Vec<Step>) -> Arc<Self> {
        let (calls, calls_rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            script,
            next: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            calls,
            calls_rx: tokio::sync::Mutex::new(calls_rx),
        })
    }

    /// The messages of every request so far.
    pub fn requests(&self) -> Vec<Vec<Message>> {
        self.requests
            .lock()
            .iter()
            .map(|r| r.messages.clone())
            .collect()
    }

    /// Every request so far, in full.
    pub fn recorded(&self) -> Vec<Recorded> {
        self.requests.lock().clone()
    }

    /// Wait for call number `n` (0-based) to start streaming.
    pub async fn wait_call(&self, n: usize) {
        let mut rx = self.calls_rx.lock().await;
        within(async {
            while let Some(index) = rx.recv().await {
                if index >= n {
                    return;
                }
            }
            panic!("call channel closed");
        })
        .await;
    }
}

#[async_trait]
impl LlmClient for ScriptedLlmClient {
    fn project_replay_messages(&self, messages: &[Message]) -> Result<Vec<Message>, LlmError> {
        Ok(messages.to_vec())
    }

    fn stream<'a>(
        &'a self,
        request: &'a LlmRequest,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<LlmEvent, LlmError>> + Send + 'a>>
    {
        let index = self.next.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().push(Recorded {
            model: request.model.clone(),
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            provider_params: request.provider_params.clone(),
            tools: request.tools.iter().map(|t| t.name.to_string()).collect(),
            schemas: request
                .tools
                .iter()
                .map(|t| t.input_schema.clone())
                .collect(),
            defs: request.tools.clone(),
            messages: request.messages.clone(),
        });
        let _ = self.calls.send(index);
        let step = self
            .script
            .get(index)
            .cloned()
            .unwrap_or(Step::Text("script exhausted"));
        let step = match step {
            Step::Dynamic(make) => make(&request.messages),
            other => other,
        };
        let (gate, step) = step.into_parts();
        let hang = matches!(step, Step::Hang);
        let events = step.events(&request.model);
        Box::pin(
            futures::stream::once(async move {
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                if hang {
                    std::future::pending::<()>().await;
                }
                futures::stream::iter(events)
            })
            .flatten(),
        )
    }

    fn provider(&self) -> Provider {
        Provider::Anthropic
    }

    async fn health_check(&self) -> Result<(), LlmError> {
        Ok(())
    }
}

/// A temporary workspace (the project root, user-config root and config dir
/// all live inside it, so nothing touches the user's home).
pub struct Workspace {
    pub dir: TempDir,
    pub root: PathBuf,
}

impl Workspace {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("project");
        std::fs::create_dir_all(&root).unwrap();
        Self { dir, root }
    }

    /// A sibling directory under the same temporary root.
    pub fn sibling(&self, name: &str) -> PathBuf {
        self.root.parent().unwrap().join(name)
    }
}

/// The config directory of the tests' workspace `root`: beside it, not in
/// it (the workspace's realm lives under it, and a realm the agent's
/// confined shell could reach refuses the start).
pub fn config_dir_for(root: &Path) -> PathBuf {
    root.parent().unwrap().join("cfg")
}

/// The realm directory of the workspace `root` (made when it has none).
pub fn realm_of(root: &Path) -> PathBuf {
    rkat_tui::bootstrap::resolve_scope(&args_for(root))
        .unwrap()
        .realm_root()
}

/// Arguments for the workspace at `root` (unconfined, no price fetch, on
/// [`MODEL`] unless `extra` names a `--model`; its config directory
/// [`config_dir_for`] unless `extra` names a `--config-dir`), plus `extra`.
pub fn args_with(root: &Path, extra: &[&str]) -> Args {
    let cfg = config_dir_for(root);
    let cfg = cfg.to_str().unwrap();
    let root = root.to_str().unwrap();
    let mut argv = vec![
        "rkat-tui",
        "-w",
        root,
        "--user-config-root",
        root,
        "--no-pricing-fetch",
    ];
    if !extra.contains(&"--config-dir") {
        argv.extend(["--config-dir", cfg]);
    }
    if !extra.contains(&"--model") {
        argv.extend(["--model", MODEL]);
    }
    if !extra.iter().any(|arg| arg.starts_with("--confinement")) {
        argv.push("--no-confinement");
    }
    argv.extend_from_slice(extra);
    Args::try_parse_from(argv).unwrap()
}

/// The endpoint file a test daemon for the workspace `root` writes and its
/// clients read: beside the workspace, not in it, and not in the user's
/// real state directory (the tests run the shell unconfined).
pub fn endpoint_file_for(root: &Path) -> PathBuf {
    root.parent().unwrap().join("state").join("mob.endpoint")
}

/// Arguments for a workspace-local realm at `root`.
pub fn args_for(root: &Path) -> Args {
    args_with(root, &[])
}

/// A model lister that lists nothing (no network).
pub fn no_lists() -> ModelLister {
    Arc::new(|endpoints: Vec<rkat_tui::endpoints::Endpoint>| {
        Box::pin(async move {
            endpoints
                .into_iter()
                .map(|e| (e.name, Ok(EndpointListing::default())))
                .collect()
        })
    })
}

/// Texts of the user messages of `messages`.
pub fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(user.text_content()),
            _ => None,
        })
        .collect()
}

/// Copy a directory tree.
pub fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else if let Err(err) = std::fs::copy(entry.path(), &target) {
            panic!("copy {}: {err}", entry.path().display());
        }
    }
}

/// The id of the task `subject` as a `task_create` result in `messages`
/// returned it.
pub fn task_id(messages: &[Message], subject: &str) -> String {
    messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResults { results, .. } => results.iter().find_map(|r| {
                let task: serde_json::Value = serde_json::from_str(&r.text_content()).ok()?;
                (task["subject"] == subject).then(|| task["id"].as_str().unwrap().to_string())
            }),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no task_create result for {subject}"))
}

/// The switch that makes a sandbox test's skip a failure (CI sets it: a
/// runner that cannot confine must fail, not pass by skipping).
pub const REQUIRE_SANDBOX: &str = "RKAT_TUI_REQUIRE_SANDBOX";

/// A sandbox test cannot run here because `reason`: say `SKIPPED:` and
/// return (the caller then returns), or fail when [`REQUIRE_SANDBOX`] is
/// `1`.
pub fn sandbox_skipped(reason: &str) {
    assert!(
        std::env::var(REQUIRE_SANDBOX).as_deref() != Ok("1"),
        "{REQUIRE_SANDBOX}=1 and the sandbox cannot run here: {reason}"
    );
    println!("SKIPPED: {reason}");
}
