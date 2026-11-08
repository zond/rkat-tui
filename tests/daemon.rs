//! The `rkat-tui-mob` daemon, started in-process in a temporary workspace
//! with a scripted LLM, driven over loopback TCP the way any client would:
//! read the endpoint file, attach with its token, then speak the engine's
//! own JSON-RPC methods.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{
    ScriptedLlmClient, Step, TIMEOUT, Workspace, args_for, args_with, endpoint_file_for, no_lists,
    within,
};
use meerkat::LlmClient;
use rkat_tui::approval::classifier::{
    ActionClassifier, ClassifierError, ClassifierInput, ClassifierVerdict,
};
use rkat_tui::bootstrap::{ApprovalOverrides, BootstrapOptions, HostPaths};
use rkat_tui::daemon::endpoint::Endpoint;
use rkat_tui::daemon::{Daemon, DaemonError, DaemonOptions, MOB_ID, ORCHESTRATOR};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

/// Allows every reviewed call: these tests drive the engine's methods,
/// not the approval gate (`tests/attach_approvals.rs` does).
struct AllowAll;

#[async_trait::async_trait]
impl ActionClassifier for AllowAll {
    async fn classify(
        &self,
        _input: ClassifierInput<'_>,
    ) -> Result<ClassifierVerdict, ClassifierError> {
        Ok(ClassifierVerdict::Allow {
            reason: "test".into(),
        })
    }
}

/// A daemon serving in the background.
struct Running {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), DaemonError>>,
    endpoint_file: std::path::PathBuf,
    session: String,
    created: bool,
}

impl Running {
    async fn start(root: &Path, llm: &Arc<ScriptedLlmClient>) -> Self {
        Self::start_args(&args_for(root), llm, None, Some(endpoint_file_for(root))).await
    }

    /// `endpoint_file` `None`: the default place, in the user's state
    /// directory.
    async fn start_args(
        args: &rkat_tui::Args,
        llm: &Arc<ScriptedLlmClient>,
        host_paths: Option<HostPaths>,
        endpoint_file: Option<std::path::PathBuf>,
    ) -> Self {
        let mut options = DaemonOptions::default().with_bootstrap(BootstrapOptions {
            llm_override: Some(Arc::clone(llm) as Arc<dyn LlmClient>),
            lister: Some(no_lists()),
            host_paths: Some(host_paths.unwrap_or_default()),
            approval: ApprovalOverrides {
                classifier: Some(Arc::new(AllowAll) as Arc<dyn ActionClassifier>),
                escalation_timeout: None,
            },
        });
        options.endpoint_file = endpoint_file;
        let daemon = within(Daemon::start(args, options))
            .await
            .expect("daemon starts");
        let endpoint_file = daemon.endpoint_file().to_path_buf();
        let session = daemon.orchestrator().session_id.to_string();
        let created = daemon.orchestrator().created;
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(daemon.serve_until(async move {
            let _ = stopped.await;
        }));
        Self {
            stop,
            task,
            endpoint_file,
            session,
            created,
        }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::read(&self.endpoint_file).unwrap()
    }

    async fn connect(&self) -> Client {
        Client::attach(&self.endpoint()).await
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        within(self.task).await.unwrap().expect("clean shutdown");
    }
}

/// A JSON-RPC client over one connection.
struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next: u64,
    notifications: Vec<Value>,
}

impl Client {
    async fn raw(endpoint: &Endpoint) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
        let stream = within(TcpStream::connect(endpoint.address)).await.unwrap();
        let (read, write) = stream.into_split();
        (BufReader::new(read), write)
    }

    async fn attach(endpoint: &Endpoint) -> Self {
        let (reader, writer) = Self::raw(endpoint).await;
        let mut client = Self {
            reader,
            writer,
            next: 100,
            notifications: Vec::new(),
        };
        let attached = client
            .call("rkat/attach", json!({"token": endpoint.token}))
            .await;
        assert_eq!(attached["result"], json!({"attached": true}), "{attached}");
        let init = client.call("initialize", json!({})).await;
        assert!(init.get("result").is_some(), "{init}");
        client
    }

    async fn send(&mut self, value: &Value) {
        let mut line = value.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn line(&mut self) -> Option<Value> {
        let mut line = String::new();
        let read = within(self.reader.read_line(&mut line)).await.unwrap();
        (read > 0).then(|| serde_json::from_str(&line).unwrap())
    }

    /// Send a request and wait for its response (notifications on the way
    /// are kept).
    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let message = self.line().await.expect("connection open");
            if message.get("id") == Some(&json!(id)) {
                return message;
            }
            self.notifications.push(message);
        }
    }

    /// The result of `method`, failing on an error response.
    async fn ok(&mut self, method: &str, params: Value) -> Value {
        let response = self.call(method, params).await;
        match response.get("result") {
            Some(result) => result.clone(),
            None => panic!("{method} failed: {response}"),
        }
    }

    /// Wait for a notification satisfying `pred`.
    async fn notification(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(found) = self.notifications.iter().find(|n| pred(n)) {
            return found.clone();
        }
        loop {
            let message = self.line().await.expect("connection open");
            self.notifications.push(message.clone());
            if pred(&message) {
                return message;
            }
        }
    }

    /// Open a stream on the orchestrator.
    async fn open_stream(&mut self) -> Value {
        let opened = self
            .ok(
                "mob/stream_open",
                json!({"mob_id": MOB_ID, "agent_identity": ORCHESTRATOR}),
            )
            .await;
        opened["stream_id"].clone()
    }

    /// Wait until the stream `stream` reports its run completed; returns the
    /// stream's events so far.
    async fn run_completed(&mut self, stream: &Value) -> Vec<Value> {
        let stream = stream.clone();
        let is_done = move |n: &Value| {
            n["method"] == "mob/stream_event"
                && n["params"]["stream_id"] == stream
                && n["params"]["event"]
                    .to_string()
                    .contains("\"type\":\"run_completed\"")
        };
        self.notification(is_done).await;
        self.notifications
            .iter()
            .filter(|n| n["method"] == "mob/stream_event")
            .cloned()
            .collect()
    }
}

fn members_include_orchestrator(result: &Value) -> bool {
    result.to_string().contains(&format!("\"{ORCHESTRATOR}\""))
}

fn turn(prompt: &str) -> Value {
    json!({"mob_id": MOB_ID, "agent_identity": ORCHESTRATOR, "prompt": prompt})
}

fn member() -> Value {
    json!({"mob_id": MOB_ID, "agent_identity": ORCHESTRATOR})
}

/// The text of the last request's messages (tool results included).
fn last_request(llm: &ScriptedLlmClient) -> String {
    format!("{:?}", llm.requests().last().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_attaches_and_finds_the_orchestrator() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    assert!(daemon.created);
    let endpoint = daemon.endpoint();
    assert!(endpoint.address.ip().is_loopback());
    assert_eq!(endpoint.pid, std::process::id());
    assert_eq!(daemon.endpoint_file, endpoint_file_for(&ws.root));
    // Nothing in the workspace holds the token (the agent's confined shell
    // can read the whole workspace).
    let holders = files_containing(&ws.root, endpoint.token.as_bytes());
    assert!(holders.is_empty(), "the token is in {holders:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&daemon.endpoint_file)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let mut client = daemon.connect().await;
    let members = client.ok("mob/members", json!({"mob_id": MOB_ID})).await;
    assert!(members_include_orchestrator(&members), "{members}");
    let status = client.ok("mob/member_status", member()).await;
    assert_eq!(
        status["current_session_id"],
        json!(daemon.session),
        "{status}"
    );
    let file = daemon.endpoint_file.clone();
    daemon.stop().await;
    assert!(!file.exists(), "the endpoint file is removed on shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_without_the_token_never_reaches_the_router() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let endpoint = daemon.endpoint();
    let requests = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 1, "method": "rkat/attach", "params": {"token": "wrong"}}),
        json!({"jsonrpc": "2.0", "id": 1, "method": "rkat/attach", "params": {}}),
    ];
    for request in requests {
        let (mut reader, mut writer) = Client::raw(&endpoint).await;
        // The refused request, then one the router would answer.
        let lines = format!(
            "{request}\n{}\n",
            json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {}})
        );
        writer.write_all(lines.as_bytes()).await.unwrap();
        let mut line = String::new();
        within(reader.read_line(&mut line)).await.unwrap();
        let refusal: Value = serde_json::from_str(&line).unwrap();
        assert!(refusal["error"]["code"].is_i64(), "{request}: {refusal}");
        assert_eq!(refusal["id"], 1, "{refusal}");
        // Then the connection is closed: `initialize` is never answered.
        let mut rest = String::new();
        let read = within(reader.read_line(&mut rest)).await.unwrap_or(0);
        assert_eq!(read, 0, "{request}: answered after the refusal: {rest}");
    }
    // The daemon still serves a client that holds the token.
    let mut client = daemon.connect().await;
    let members = client.ok("mob/members", json!({"mob_id": MOB_ID})).await;
    assert!(members_include_orchestrator(&members), "{members}");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_streams_to_every_attached_client() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![Step::Text("hello from the orchestrator")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut first = daemon.connect().await;
    let mut second = daemon.connect().await;
    let first_stream = first.open_stream().await;
    let second_stream = second.open_stream().await;
    first.ok("mob/turn_start", turn("say hello")).await;
    for (client, stream) in [(&mut first, &first_stream), (&mut second, &second_stream)] {
        let events = client.run_completed(stream).await;
        let text = Value::Array(events).to_string();
        assert!(text.contains("hello from the orchestrator"), "{text}");
    }
    assert!(llm.recorded()[0].user().contains("say hello"));
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_leaving_mid_turn_leaves_the_turn_running() {
    let ws = Workspace::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let llm = ScriptedLlmClient::new(vec![Step::Gated(
        Arc::clone(&gate),
        Box::new(Step::Text("finished without you")),
    )]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = daemon.connect().await;
    let _ = client.open_stream().await;
    // `mob/turn_start` is request-scoped: it answers when the turn ends,
    // and its connection closing cancels the run. `mob/member_send` hands
    // the input to the member and answers at once: the run is the
    // member's and outlives the client.
    let sent = client
        .ok(
            "mob/member_send",
            json!({"mob_id": MOB_ID, "agent_identity": ORCHESTRATOR, "content": "long work"}),
        )
        .await;
    assert_eq!(sent["agent_identity"], ORCHESTRATOR, "{sent}");
    llm.wait_call(0).await;
    drop(client);
    gate.notify_one();
    // A new client catches up from the member's durable session events
    // (`mob/stream_open` has no cursor): the turn ran to its end.
    let mut client = daemon.connect().await;
    let status = client.ok("mob/member_status", member()).await;
    assert_eq!(
        status["current_session_id"],
        json!(daemon.session),
        "{status}"
    );
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let since = client
            .ok(
                "events/list_since",
                json!({"scope": {"type": "session", "session_id": daemon.session}, "limit": 500}),
            )
            .await;
        let text = since.to_string();
        if text.contains("\"type\":\"run_completed\"") {
            assert!(text.contains("finished without you"), "{text}");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn never completed: {text}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_orchestrator_has_the_tui_file_tools() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("notes.txt"), "the file content 4711\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "r1",
            name: "read_file",
            args: json!({"path": "notes.txt"}),
        },
        Step::Text("read it"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    let mut client = daemon.connect().await;
    let stream = client.open_stream().await;
    client.ok("mob/turn_start", turn("read notes.txt")).await;
    client.run_completed(&stream).await;
    assert!(llm.recorded()[0].tools.iter().any(|t| t == "read_file"));
    let request = last_request(&llm);
    assert!(request.contains("the file content 4711"), "{request}");
    daemon.stop().await;
}

/// Why this host cannot run a confined launch over `workspace`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn sandbox_unavailable(workspace: &Path) -> Option<String> {
    use rkat_tui::bootstrap::confinement::lower;
    use rkat_tui::cli::ConfinementArg;
    let requirement = match lower(ConfinementArg::Workspace, workspace, &HostPaths::detect()) {
        Ok(Some(requirement)) => requirement,
        Ok(None) => return None,
        Err(err) => return Some(format!("the requirement is invalid: {err}")),
    };
    let launch = match meerkat_sandbox::ProcessLaunchSpec::new(
        "/bin/true".into(),
        Vec::new(),
        workspace.to_path_buf(),
        std::collections::BTreeMap::new(),
    ) {
        Ok(launch) => launch,
        Err(refusal) => return Some(format!("the probe launch is invalid: {refusal}")),
    };
    let prepared = match meerkat_sandbox::prepare(&requirement, launch) {
        Ok(prepared) => prepared,
        Err(refusal) => return Some(format!("the backend refuses the requirement: {refusal}")),
    };
    match prepared.spawn() {
        Ok(mut child) => match child.wait().await {
            Ok(status) if status.success() => None,
            Ok(status) => Some(format!("a confined /bin/true exited with {status}")),
            Err(err) => Some(format!("a confined launch cannot be awaited: {err}")),
        },
        Err(err) => Some(format!("a confined launch cannot start: {err}")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn sandbox_unavailable(_workspace: &Path) -> Option<String> {
    Some("this platform has no sandbox backend".to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_orchestrator_shell_runs_confined() {
    let ws = Workspace::new();
    if let Some(reason) = sandbox_unavailable(&ws.root).await {
        println!("SKIPPED: --confinement=workspace is unavailable on this host: {reason}");
        return;
    }
    // The temp dir is granted to the shell here, so the endpoint file
    // goes to its default place in the user's state directory.
    let default = rkat_tui::daemon::endpoint_file(&ws.root).unwrap();
    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "sh",
            name: "shell",
            args: json!({"command": format!(
                "echo confined-$((40+2)); cat '{}' 2>&1; ls -a '{}' 2>&1",
                default.display(),
                ws.root.join(".rkat").display(),
            )}),
        },
        Step::Text("ran it"),
    ]);
    let args = args_with(&ws.root, &["--confinement=workspace"]);
    let daemon = Running::start_args(&args, &llm, Some(HostPaths::detect()), None).await;
    assert_eq!(daemon.endpoint_file, default);
    let token = daemon.endpoint().token;
    let mut client = daemon.connect().await;
    let stream = client.open_stream().await;
    client.ok("mob/turn_start", turn("echo")).await;
    client.run_completed(&stream).await;
    let request = last_request(&llm);
    assert!(request.contains("confined-42"), "{request}");
    // The agent's shell cannot read the attach token.
    assert!(!request.contains(&token), "{request}");
    daemon.stop().await;
    if let Some(dir) = default.parent() {
        let _ = std::fs::remove_dir(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_endpoint_file_the_confined_shell_could_read_is_refused() {
    let ws = Workspace::new();
    if let Some(reason) = sandbox_unavailable(&ws.root).await {
        println!("SKIPPED: --confinement=workspace is unavailable on this host: {reason}");
        return;
    }
    let llm = ScriptedLlmClient::new(vec![]);
    let inside = ws.root.join(".rkat").join("tui").join("mob.endpoint");
    let options = DaemonOptions::default()
        .with_bootstrap(BootstrapOptions {
            llm_override: Some(Arc::clone(&llm) as Arc<dyn LlmClient>),
            lister: Some(no_lists()),
            host_paths: Some(HostPaths::default()),
            ..BootstrapOptions::default()
        })
        .with_endpoint_file(inside.clone());
    let args = args_with(&ws.root, &["--confinement=workspace"]);
    let refused = within(Daemon::start(&args, options)).await;
    assert!(
        matches!(&refused, Err(DaemonError::EndpointExposed { file, grant })
            if *file == inside && *grant == ws.root),
        "{refused:?}"
    );
    assert!(!inside.exists());
}

/// The files under `dir` whose content contains `needle`.
fn files_containing(dir: &Path, needle: &[u8]) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if std::fs::read(&path)
                .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
            {
                found.push(path);
            }
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_daemon_restores_the_orchestrator_with_its_tools() {
    let ws = Workspace::new();
    std::fs::write(ws.root.join("after.txt"), "after the restart 1234\n").unwrap();
    let llm = ScriptedLlmClient::new(vec![Step::Text("first life")]);
    let daemon = Running::start(&ws.root, &llm).await;
    let first_session = daemon.session.clone();
    let mut client = daemon.connect().await;
    let stream = client.open_stream().await;
    client.ok("mob/turn_start", turn("one")).await;
    client.run_completed(&stream).await;
    drop(client);
    daemon.stop().await;

    let llm = ScriptedLlmClient::new(vec![
        Step::Tool {
            id: "r1",
            name: "read_file",
            args: json!({"path": "after.txt"}),
        },
        Step::Text("second life"),
    ]);
    let daemon = Running::start(&ws.root, &llm).await;
    assert!(
        !daemon.created,
        "the orchestrator is restored, not spawned again"
    );
    assert_eq!(daemon.session, first_session);
    let mut client = daemon.connect().await;
    let members = client.ok("mob/members", json!({"mob_id": MOB_ID})).await;
    assert!(members_include_orchestrator(&members), "{members}");
    let stream = client.open_stream().await;
    client.ok("mob/turn_start", turn("two")).await;
    let events = client.run_completed(&stream).await;
    assert!(Value::Array(events).to_string().contains("second life"));
    let request = last_request(&llm);
    assert!(request.contains("after the restart 1234"), "{request}");
    // The restored session carries the first turn.
    assert!(request.contains("first life"), "{request}");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_daemon_on_a_live_workspace_is_refused() {
    let ws = Workspace::new();
    let llm = ScriptedLlmClient::new(vec![]);
    let daemon = Running::start(&ws.root, &llm).await;
    let options = DaemonOptions::default()
        .with_bootstrap(BootstrapOptions {
            llm_override: Some(Arc::clone(&llm) as Arc<dyn LlmClient>),
            lister: Some(no_lists()),
            host_paths: Some(HostPaths::default()),
            ..BootstrapOptions::default()
        })
        .with_endpoint_file(endpoint_file_for(&ws.root));
    let refused = within(Daemon::start(&args_for(&ws.root), options)).await;
    assert!(
        matches!(refused, Err(DaemonError::AlreadyRunning { .. })),
        "{refused:?}"
    );
    daemon.stop().await;
}

/// The upgrade path: an earlier version wrote its endpoint file (and its
/// token) into `<workspace>/.rkat/tui`. A daemon of that version that still
/// answers keeps the workspace (the new daemon refuses, naming its pid); a
/// stale file is removed at start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_legacy_endpoint_in_the_workspace_refuses_while_live_and_is_removed_when_stale() {
    let ws = Workspace::new();
    let legacy = rkat_tui::daemon::endpoint::legacy_file(&ws.root);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    Endpoint {
        address: listener.local_addr().unwrap(),
        token: "old".into(),
        pid: 31337,
    }
    .write(&legacy)
    .unwrap();
    let llm = ScriptedLlmClient::new(vec![]);
    let options = || {
        DaemonOptions::default()
            .with_bootstrap(BootstrapOptions {
                llm_override: Some(Arc::clone(&llm) as Arc<dyn LlmClient>),
                lister: Some(no_lists()),
                host_paths: Some(HostPaths::default()),
                ..BootstrapOptions::default()
            })
            .with_endpoint_file(endpoint_file_for(&ws.root))
    };
    let refused = within(Daemon::start(&args_for(&ws.root), options())).await;
    assert!(
        matches!(&refused, Err(DaemonError::AlreadyRunning { pid: 31337, file, .. }) if *file == legacy),
        "{refused:?}"
    );
    assert!(legacy.exists());

    drop(listener);
    let daemon = Running::start(&ws.root, &llm).await;
    assert!(
        !legacy.exists(),
        "the stale legacy endpoint file (a token the shell can read) is removed"
    );
    daemon.stop().await;
}
