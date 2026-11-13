//! The attach-mode rig shared by `tests/client.rs`,
//! `tests/approvals.rs`, `tests/helpers.rs`,
//! `tests/conversations.rs` and `tests/daemon.rs`: a daemon started
//! inside the test process with a scripted LLM (an allow-all classifier unless the test
//! passes its own options), and attach runners driven through the UI's
//! `Effect`/`Msg` channels. `tests/daemon.rs` keeps its own raw JSON-RPC
//! client, which tests the wire below the attach runner.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meerkat::LlmClient;
use meerkat_core::AgentEvent;
use rkat_tui::approval::classifier::{
    ActionClassifier, ClassifierError, ClassifierInput, ClassifierVerdict,
};
use rkat_tui::approval::escalation::RecordedPrompt;
use rkat_tui::approval::mode::PermissionMode;
use rkat_tui::bootstrap::{ApprovalOverrides, BootstrapOptions, HostPaths};
use rkat_tui::cli::AttachTarget;
use rkat_tui::client::testing::{AttachHarness, spawn_attach_runner};
use rkat_tui::client::transport::{Connection, Notification};
use rkat_tui::daemon::approvals::DaemonToolOrigins;
use rkat_tui::daemon::endpoint::Endpoint;
use rkat_tui::daemon::{Daemon, DaemonError, DaemonOptions};
use rkat_tui::ui::{Effect, EscalationId, Msg, StatusUpdate, TurnFailure, TurnSummary};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::common::{ScriptedLlmClient, TIMEOUT, args_for, endpoint_file_for, no_lists, within};

/// Allows every reviewed call (these tests are not about the gate).
pub struct AllowAll;

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
pub struct Running {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), DaemonError>>,
    pub endpoint_file: PathBuf,
    /// The agent the daemon started with (`main-<id>`).
    pub agent: String,
    pub session: meerkat_core::SessionId,
    /// Whether the agent was created (not restored) at start.
    pub created: bool,
    pub origins: Arc<DaemonToolOrigins>,
    /// The daemon's mob host, helper bookkeeping and approval engine,
    /// held weakly so the test never keeps them past the daemon's stop
    /// (see [`Self::state`]).
    state: std::sync::Weak<meerkat_mob_mcp::MobMcpState>,
    registry: std::sync::Weak<rkat_tui::mobs::HelperRegistry>,
    engine: std::sync::Weak<rkat_tui::approval::ApprovalEngine>,
    local: std::sync::Weak<rkat_tui::daemon::local::Local>,
    /// The engine's router past the proxy's allow-list.
    pub engine_side: rkat_tui::daemon::testing::EngineSide,
    /// The agent profile's auto-compaction threshold.
    pub compact_threshold: Option<std::num::NonZeroU64>,
}

impl Running {
    /// The bootstrap of a test daemon: the scripted LLM, no model lists,
    /// the allow-all classifier.
    pub fn bootstrap(llm: &Arc<ScriptedLlmClient>) -> BootstrapOptions {
        BootstrapOptions {
            llm_override: Some(Arc::clone(llm) as Arc<dyn LlmClient>),
            lister: Some(no_lists()),
            host_paths: Some(HostPaths::default()),
            approval: ApprovalOverrides {
                classifier: Some(Arc::new(AllowAll) as Arc<dyn ActionClassifier>),
                escalation_timeout: None,
            },
            without_token_store: true,
        }
    }

    /// The options of a test daemon: [`Self::bootstrap`], its endpoint
    /// file outside the workspace.
    pub fn options(root: &Path, llm: &Arc<ScriptedLlmClient>) -> DaemonOptions {
        DaemonOptions::default()
            .with_bootstrap(Self::bootstrap(llm))
            .with_endpoint_file(endpoint_file_for(root))
    }

    pub async fn start(root: &Path, llm: &Arc<ScriptedLlmClient>) -> Self {
        Self::start_with(&args_for(root), Self::options(root, llm)).await
    }

    /// Start with these arguments and options.
    pub async fn start_with(args: &rkat_tui::Args, options: DaemonOptions) -> Self {
        Self::try_start_with(args, options)
            .await
            .expect("daemon starts")
    }

    /// [`Self::start_with`], returning a start-up failure.
    pub async fn try_start_with(
        args: &rkat_tui::Args,
        options: DaemonOptions,
    ) -> Result<Self, DaemonError> {
        within(Daemon::start(args, options)).await.map(Self::serve)
    }

    /// Serve a started daemon in the background.
    pub fn serve(daemon: Daemon) -> Self {
        let endpoint_file = daemon.endpoint_file().to_path_buf();
        let session = daemon.first_agent().session_id.clone();
        let agent = daemon.first_agent().identity.to_string();
        let created = daemon.first_agent().created;
        let origins = Arc::clone(daemon.tool_origins());
        let state = Arc::downgrade(daemon.mob_state());
        let registry = Arc::downgrade(daemon.helper_registry());
        let engine = Arc::downgrade(daemon.approval_engine());
        let local = Arc::downgrade(daemon.local());
        let engine_side = daemon.engine_side();
        let compact_threshold = daemon.compact_threshold();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(daemon.serve_until(async move {
            let _ = stopped.await;
        }));
        Self {
            stop,
            task,
            endpoint_file,
            agent,
            session,
            created,
            origins,
            state,
            registry,
            engine,
            local,
            engine_side,
            compact_threshold,
        }
    }

    /// The agents' hidden tools and their reveals.
    #[allow(dead_code)]
    pub fn deferral(&self) -> Arc<rkat_tui::daemon::deferral::Deferral> {
        Arc::clone(self.local.upgrade().expect("the daemon runs").deferral())
    }

    /// Reveal `names` for `session` as a `tool_search` would (tests about
    /// the revealed tools themselves, not about finding them:
    /// `tests/deferral.rs` covers the search).
    #[allow(dead_code)]
    pub async fn reveal(&self, session: &meerkat_core::SessionId, names: &[&str]) {
        let names: Vec<String> = names.iter().map(ToString::to_string).collect();
        self.deferral()
            .reveals
            .reveal(session, &names)
            .await
            .expect("the reveal is written");
    }

    /// How many `method` requests the clients sent the engine through the
    /// daemon's proxy so far.
    pub fn forwarded(&self, method: &str) -> usize {
        self.local
            .upgrade()
            .expect("the daemon serves")
            .forwarded(method)
    }

    /// The daemon's mob host (while it serves).
    pub fn state(&self) -> Arc<meerkat_mob_mcp::MobMcpState> {
        self.state.upgrade().expect("the daemon serves")
    }

    /// The daemon's helper bookkeeping (while it serves).
    pub fn registry(&self) -> Arc<rkat_tui::mobs::HelperRegistry> {
        self.registry.upgrade().expect("the daemon serves")
    }

    /// The daemon's approval engine (while it serves).
    pub fn engine(&self) -> Arc<rkat_tui::approval::ApprovalEngine> {
        self.engine.upgrade().expect("the daemon serves")
    }

    /// The daemon's endpoint.
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::read(&self.endpoint_file).unwrap()
    }

    /// A raw connection to the daemon (not initialized) and its
    /// notifications. It attaches as a probe: not counted as a client and
    /// bound to no agent (its `rkat/*` calls act on the most recently
    /// active one), so a test's look at the daemon leaves no agent
    /// detached when it closes.
    pub async fn connect_with_notes(&self) -> (Connection, UnboundedReceiver<Notification>) {
        Connection::attach_as(&self.endpoint(), Duration::from_secs(5), None, true)
            .await
            .unwrap()
    }

    /// A raw connection to the daemon.
    pub async fn connect(&self) -> Connection {
        let (connection, _notes) = self.connect_with_notes().await;
        connection
            .call("initialize", serde_json::json!({}))
            .await
            .unwrap();
        connection
    }

    /// A raw connection bound to the daemon's start agent, initialized: a
    /// client of that agent (counted, its end a detach), which the daemon
    /// lets drive the agent through the engine's member methods (a probe
    /// is bound to no agent and may only read).
    pub async fn connect_bound(&self) -> Connection {
        self.connect_bound_with_notes().await.0
    }

    /// [`Self::connect_bound`], with the connection's pushes.
    pub async fn connect_bound_with_notes(&self) -> (Connection, UnboundedReceiver<Notification>) {
        let (connection, notes) = Connection::attach_to(
            &self.endpoint(),
            Duration::from_secs(5),
            None,
            false,
            Some(rkat_tui::daemon::protocol::AgentChoice::Agent {
                name: self.agent.clone(),
            }),
        )
        .await
        .unwrap();
        connection
            .call("initialize", serde_json::json!({}))
            .await
            .unwrap();
        (connection, notes)
    }

    /// Wait for the daemon to end by itself (`rkat/shutdown`).
    pub async fn ended(self) {
        within(self.task).await.unwrap().expect("clean shutdown");
    }

    /// Whether the daemon still serves (it has not ended).
    pub fn serving(&self) -> bool {
        !self.task.is_finished()
    }

    pub async fn stop(self) {
        let _ = self.stop.send(());
        within(self.task).await.unwrap().expect("clean shutdown");
    }
}

/// An attach runner and everything it said.
pub struct Client {
    pub harness: AttachHarness,
    pub seen: Vec<Msg>,
}

impl Client {
    pub async fn attach(root: &Path) -> Self {
        Self::attach_with(root, None).await
    }

    /// Attach presenting `start_token` (the client that started the daemon).
    pub async fn attach_with(root: &Path, start_token: Option<&str>) -> Self {
        Self::attach_args(root, &args_for(root), start_token).await
    }

    /// Attach working with the agent `choice` names (a new one, …).
    pub async fn attach_agent(
        root: &Path,
        choice: rkat_tui::daemon::protocol::AgentChoice,
    ) -> Self {
        Self::attach_config(
            &args_for(root),
            None,
            no_lists(),
            &endpoint_file_for(root),
            Some(choice),
        )
        .await
    }

    /// Attach with these arguments (`-c`, `--resume`, …).
    pub async fn attach_args(
        root: &Path,
        args: &rkat_tui::Args,
        start_token: Option<&str>,
    ) -> Self {
        Self::attach_listing(root, args, start_token, no_lists()).await
    }

    /// [`Self::attach_args`] with the model lister the picker uses.
    pub async fn attach_listing(
        root: &Path,
        args: &rkat_tui::Args,
        start_token: Option<&str>,
        lister: rkat_tui::models::ModelLister,
    ) -> Self {
        Self::attach_setup(args, start_token, lister, &endpoint_file_for(root)).await
    }

    /// Attach through `endpoint_file`.
    pub async fn attach_setup(
        args: &rkat_tui::Args,
        start_token: Option<&str>,
        lister: rkat_tui::models::ModelLister,
        endpoint_file: &Path,
    ) -> Self {
        Self::attach_config(args, start_token, lister, endpoint_file, None).await
    }

    /// [`Self::attach_setup`], asking for the agent `agent` names. `None`:
    /// as the command line says, except that a command line with neither
    /// `-c` nor `--resume` joins the most recent agent (as `-c` does) rather
    /// than starting a new one: the rig's clients share the daemon's agent
    /// unless a test asks otherwise (`Some(AgentChoice::New)`, or
    /// `Some(rkat_tui::client::agent_choice(&args))` for the real default).
    pub async fn attach_config(
        args: &rkat_tui::Args,
        start_token: Option<&str>,
        lister: rkat_tui::models::ModelLister,
        endpoint_file: &Path,
        agent: Option<rkat_tui::daemon::protocol::AgentChoice>,
    ) -> Self {
        let config = client_config(args, start_token, lister, endpoint_file, agent).await;
        Self::attach_with_config(config).await
    }

    /// Attach a runner on `config`.
    pub async fn attach_with_config(config: rkat_tui::client::ClientConfig) -> Self {
        let mut client = Self {
            harness: spawn_attach_runner(config),
            seen: Vec::new(),
        };
        client.attached().await;
        client
    }

    pub fn send(&self, effect: Effect) {
        self.harness.effects.send(effect).unwrap();
    }

    /// Wait for a message matching `pred` (seen before or arriving).
    pub async fn saw(&mut self, pred: impl Fn(&Msg) -> bool) {
        if !self.seen.iter().any(&pred) {
            self.wait_for(pred).await;
        }
    }

    pub async fn wait_for(&mut self, pred: impl Fn(&Msg) -> bool) -> Msg {
        let seen = &mut self.seen;
        let rx = &mut self.harness.ui_rx;
        let found = tokio::time::timeout(TIMEOUT, async {
            loop {
                let msg = rx.recv().await.expect("runner gone");
                seen.push(msg.clone());
                if pred(&msg) {
                    return msg;
                }
            }
        })
        .await;
        match found {
            Ok(msg) => msg,
            Err(_) => {
                let tail = &self.seen[self.seen.len().saturating_sub(20)..];
                panic!("timed out; the last messages: {tail:#?}")
            }
        }
    }

    pub async fn attached(&mut self) {
        self.wait_for(|msg| {
            matches!(
                msg,
                Msg::Status(StatusUpdate::Connection(
                    rkat_tui::ui::Connection::Attached { .. }
                ))
            )
        })
        .await;
    }

    /// The agent this client works with (the status bar's).
    pub async fn agent(&mut self) -> String {
        let shown = |msg: &Msg| match msg {
            Msg::Status(StatusUpdate::Agent(Some(agent))) => Some(agent.clone()),
            _ => None,
        };
        if let Some(agent) = self.seen.iter().rev().find_map(shown) {
            return agent;
        }
        let msg = self.wait_for(|msg| shown(msg).is_some()).await;
        shown(&msg).unwrap()
    }

    pub async fn turn_finished(&mut self) -> Result<TurnSummary, TurnFailure> {
        match self
            .wait_for(|msg| matches!(msg, Msg::TurnFinished(_)))
            .await
        {
            Msg::TurnFinished(outcome) => outcome,
            _ => unreachable!(),
        }
    }

    /// The agent events seen so far.
    pub fn events(&self) -> Vec<&AgentEvent> {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                Msg::Agent(env) => Some(&env.payload),
                _ => None,
            })
            .collect()
    }

    /// How many `text_complete` events with `text` were seen.
    pub fn texts(&self, text: &str) -> usize {
        self.events()
            .iter()
            .filter(|event| matches!(event, AgentEvent::TextComplete { content, .. } if content == text))
            .count()
    }

    pub async fn escalation(&mut self) -> (RecordedPrompt, EscalationId) {
        match self
            .wait_for(|msg| matches!(msg, Msg::Escalation(..)))
            .await
        {
            Msg::Escalation(prompt, id) => (prompt, id),
            _ => unreachable!(),
        }
    }

    /// How many approval dialogs were shown.
    pub fn escalations(&self) -> usize {
        self.seen
            .iter()
            .filter(|msg| matches!(msg, Msg::Escalation(..)))
            .count()
    }

    pub async fn set_mode(&mut self, mode: PermissionMode) {
        self.send(Effect::SetMode(mode));
        self.wait_for(|msg| {
            matches!(msg, Msg::Status(StatusUpdate::Mode { mode: shown, .. }) if *shown == mode)
        })
        .await;
    }

    pub fn notices(&self) -> String {
        self.seen
            .iter()
            .filter_map(|msg| match msg {
                Msg::Notice { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Quit; where the messages the daemon never got were saved.
    pub async fn quit(self) -> Option<rkat_tui::workbench::SavedUnsent> {
        self.send(Effect::Quit);
        within(self.harness.task).await.unwrap()
    }
}

/// The configuration of a rig client: see [`Client::attach_config`].
pub async fn client_config(
    args: &rkat_tui::Args,
    start_token: Option<&str>,
    lister: rkat_tui::models::ModelLister,
    endpoint_file: &Path,
    agent: Option<rkat_tui::daemon::protocol::AgentChoice>,
) -> rkat_tui::client::ClientConfig {
    let mut config = rkat_tui::client::configure(args, AttachTarget::Existing)
        .await
        .unwrap();
    config.start_token = start_token.map(str::to_string);
    match agent {
        Some(agent) => config.agent = agent,
        None if args.resume.is_none() && !args.continue_last => {
            config.agent = rkat_tui::daemon::protocol::AgentChoice::Recent;
        }
        None => {}
    }
    config.lister = lister;
    config.endpoint_files = vec![endpoint_file.to_path_buf()];
    config
}

/// The daemons a re-home started in the rig, in order.
pub type Restarted = Arc<parking_lot::Mutex<Vec<Running>>>;

/// How a rig client on the command line `args` re-roots on `/rehome`: the
/// configuration for a workspace starts the rig's daemon there (with
/// `llm`, its endpoint file at `files(workspace)`, which is also where the
/// re-home looks for a workspace's daemon) and attaches through it; the
/// daemons it starts go into the returned list. The daemon gets the
/// command line the runner passes (`args` with the new workspace), so its
/// user config root, where the engine keeps the comms identity the
/// realm's sessions are bound to, stays the one it was, as the home
/// directory stays in production.
pub fn reroot(
    args: rkat_tui::Args,
    llm: &Arc<ScriptedLlmClient>,
    files: impl Fn(&Path) -> PathBuf + Send + Sync + 'static,
) -> (rkat_tui::client::runner::Reroot, Restarted) {
    let restarted = Restarted::default();
    let files = Arc::new(files);
    let configure = {
        let llm = Arc::clone(llm);
        let restarted = Arc::clone(&restarted);
        let files = Arc::clone(&files);
        Arc::new(move |args: rkat_tui::Args| {
            let llm = Arc::clone(&llm);
            let restarted = Arc::clone(&restarted);
            let files = Arc::clone(&files);
            Box::pin(async move {
                let workspace = args
                    .context_root
                    .clone()
                    .expect("a re-home names the workspace");
                let file = files(&workspace);
                let daemon_args = args;
                let options = Running::options(&workspace, &llm).with_endpoint_file(file.clone());
                let daemon = Running::start_with(&daemon_args, options).await;
                restarted.lock().push(daemon);
                Ok(client_config(&daemon_args, None, no_lists(), &file, None).await)
            })
                as std::pin::Pin<
                    Box<
                        dyn std::future::Future<
                                Output = Result<rkat_tui::client::ClientConfig, rkat_tui::TuiError>,
                            > + Send,
                    >,
                >
        })
    };
    let reroot = rkat_tui::client::runner::Reroot {
        args,
        configure,
        endpoint_files: Arc::new(move |workspace: &Path| vec![files(workspace)]),
    };
    (reroot, restarted)
}

/// What a daemon test starts with besides its scripted LLM: extra
/// command-line arguments (given to the daemon and the client alike), the
/// classifier (default [`AllowAll`]), the escalation timeout, the model
/// lister (default none) and the host paths a confined shell may use
/// (default none).
#[derive(Clone, Default)]
pub struct Setup {
    pub args: Vec<String>,
    /// Arguments only the client gets (`-c`, `--resume`; the daemon
    /// refuses them).
    pub client_args: Vec<String>,
    pub classifier: Option<Arc<dyn ActionClassifier>>,
    /// The bootstrap's own model classifier on the scripted LLM (its
    /// calls take the script's next steps) instead of `classifier`.
    pub llm_classifier: bool,
    pub escalation_timeout: Option<Duration>,
    pub lister: Option<rkat_tui::models::ModelLister>,
    pub host_paths: Option<HostPaths>,
    /// Run every session on the engine's own clients (against a fake
    /// endpoint) instead of the scripted LLM; the user's token store stays
    /// closed either way.
    pub engine_clients: bool,
    /// Where the endpoint file goes (default [`endpoint_file_for`] the
    /// workspace; a test whose shell is granted the temp dir puts it
    /// elsewhere, or the daemon refuses it as exposed).
    pub endpoint_file: Option<PathBuf>,
}

impl Setup {
    /// These extra arguments, everything else by default.
    pub fn args(args: &[&str]) -> Self {
        Self {
            args: args.iter().map(ToString::to_string).collect(),
            ..Self::default()
        }
    }

    /// With this classifier.
    pub fn classifier(mut self, classifier: Arc<dyn ActionClassifier>) -> Self {
        self.classifier = Some(classifier);
        self
    }

    /// The daemon's options for `root` with `llm`.
    pub fn options(&self, root: &Path, llm: &Arc<ScriptedLlmClient>) -> DaemonOptions {
        DaemonOptions::default()
            .with_bootstrap(BootstrapOptions {
                // A setup on the engine's own clients (a fake endpoint)
                // has no scripted LLM.
                llm_override: (!self.engine_clients).then(|| Arc::clone(llm) as Arc<dyn LlmClient>),
                without_token_store: true,
                lister: Some(self.lister.clone().unwrap_or_else(no_lists)),
                host_paths: Some(self.host_paths.clone().unwrap_or_default()),
                approval: ApprovalOverrides {
                    classifier: (!self.llm_classifier).then(|| {
                        self.classifier
                            .clone()
                            .unwrap_or_else(|| Arc::new(AllowAll) as Arc<dyn ActionClassifier>)
                    }),
                    escalation_timeout: self.escalation_timeout,
                },
            })
            .with_endpoint_file(self.endpoint_file(root))
    }

    /// The endpoint file of a daemon on `root`.
    pub fn endpoint_file(&self, root: &Path) -> PathBuf {
        self.endpoint_file
            .clone()
            .unwrap_or_else(|| endpoint_file_for(root))
    }

    /// The command line for `root`.
    pub fn command_line(&self, root: &Path) -> rkat_tui::Args {
        let extra: Vec<&str> = self.args.iter().map(String::as_str).collect();
        crate::common::args_with(root, &extra)
    }
}

/// The headless harness: a daemon on a workspace with a scripted LLM and
/// one client attached to it, driven through the UI's `Effect`/`Msg`
/// channels. Dereferences to the [`Client`].
pub struct Attached {
    pub daemon: Running,
    pub client: Client,
}

impl Attached {
    /// A daemon on `root` with `llm` and the default [`Setup`], and a client.
    pub async fn start(root: &Path, llm: &Arc<ScriptedLlmClient>) -> Self {
        Self::start_with(root, llm, Setup::default()).await
    }

    /// A daemon on `root` with `llm` and `setup`, and a client.
    pub async fn start_with(root: &Path, llm: &Arc<ScriptedLlmClient>, setup: Setup) -> Self {
        Self::try_start_with(root, llm, setup)
            .await
            .expect("daemon starts")
    }

    /// [`Self::start_with`], returning the daemon's start-up failure.
    pub async fn try_start_with(
        root: &Path,
        llm: &Arc<ScriptedLlmClient>,
        setup: Setup,
    ) -> Result<Self, DaemonError> {
        let args = setup.command_line(root);
        let daemon = Running::try_start_with(&args, setup.options(root, llm)).await?;
        let lister = setup.lister.clone().unwrap_or_else(no_lists);
        let mut client_args = setup.args.clone();
        client_args.extend(setup.client_args.iter().cloned());
        let client_args: Vec<&str> = client_args.iter().map(String::as_str).collect();
        let client_args = crate::common::args_with(root, &client_args);
        let client =
            Client::attach_setup(&client_args, None, lister, &setup.endpoint_file(root)).await;
        Ok(Self { daemon, client })
    }

    /// The session the agent runs as now.
    pub async fn current_session(&self) -> meerkat_core::SessionId {
        self.daemon.current_session().await
    }

    /// The committed messages of the current conversation.
    pub async fn messages(&self) -> Vec<meerkat_core::Message> {
        let session = self.current_session().await;
        self.daemon.transcript(&session).await
    }

    /// The client quits, then the daemon stops.
    pub async fn quit(self) -> Option<rkat_tui::workbench::SavedUnsent> {
        let unsent = self.client.quit().await;
        self.daemon.stop().await;
        unsent
    }
}

impl std::ops::Deref for Attached {
    type Target = Client;
    fn deref(&self) -> &Client {
        &self.client
    }
}

impl std::ops::DerefMut for Attached {
    fn deref_mut(&mut self) -> &mut Client {
        &mut self.client
    }
}

impl Running {
    /// The session the daemon's first agent runs as now
    /// (`mob/member_status`).
    pub async fn current_session(&self) -> meerkat_core::SessionId {
        self.session_of(&self.agent).await
    }

    /// The session the agent `agent` runs as now (`mob/member_status`).
    pub async fn session_of(&self, agent: &str) -> meerkat_core::SessionId {
        let status = self
            .connect()
            .await
            .call(
                "mob/member_status",
                serde_json::json!({"mob_id": "tui", "agent_identity": agent}),
            )
            .await
            .unwrap();
        serde_json::from_value(status["current_session_id"].clone()).unwrap()
    }

    /// The daemon's `rkat/status` as a client bound to no agent sees it.
    pub async fn status(&self) -> rkat_tui::daemon::protocol::Status {
        let answer = self
            .connect()
            .await
            .call("rkat/status", serde_json::json!({}))
            .await
            .unwrap();
        serde_json::from_value(answer).unwrap()
    }

    /// Wait (bounded by [`within`]) until the daemon reports
    /// `agent`'s turn idle: a run it started on its own (a schedule, a
    /// watch) has ended, so a client attaching now does not join it.
    pub async fn idle(&self, agent: &str) {
        within(async {
            loop {
                let idle = self
                    .status()
                    .await
                    .agents
                    .iter()
                    .any(|row| row.name == agent && row.running == Some(false));
                if idle {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
    }

    /// The committed messages of `session` (`rkat/transcript`).
    pub async fn transcript(
        &self,
        session: &meerkat_core::SessionId,
    ) -> Vec<meerkat_core::Message> {
        let answer = self
            .connect()
            .await
            .call(
                "rkat/transcript",
                serde_json::json!({"session_id": session.to_string()}),
            )
            .await
            .unwrap();
        serde_json::from_value(answer["messages"].clone()).unwrap()
    }
}

/// The session `id` as the realm of the workspace at `root` stores it (read
/// from the store directly, beside or after the daemon).
pub async fn stored_session(root: &Path, id: &meerkat_core::SessionId) -> meerkat_core::Session {
    let scope = rkat_tui::bootstrap::resolve_scope(&args_for(root)).unwrap();
    let (_manifest, persistence) =
        meerkat::open_realm_persistence_in(&scope.realms_root, &scope.realm_id, None, None)
            .await
            .unwrap();
    persistence
        .session_store()
        .load(id)
        .await
        .unwrap()
        .expect("the session is stored")
}

/// A classifier that replays verdicts and records what it was shown. When
/// the script runs out it fails ([`Self::new`]) or allows
/// ([`Self::allowing`]).
#[derive(Default)]
pub struct ScriptedClassifier {
    verdicts: parking_lot::Mutex<Vec<Result<ClassifierVerdict, ClassifierError>>>,
    seen: parking_lot::Mutex<Vec<(String, serde_json::Value, String)>>,
    allow_when_done: bool,
}

impl ScriptedClassifier {
    /// These verdicts, then a provider error.
    pub fn new(verdicts: Vec<Result<ClassifierVerdict, ClassifierError>>) -> Arc<Self> {
        Arc::new(Self {
            verdicts: parking_lot::Mutex::new(verdicts),
            ..Self::default()
        })
    }

    /// These verdicts, then allow.
    pub fn allowing(verdicts: Vec<ClassifierVerdict>) -> Arc<Self> {
        Arc::new(Self {
            verdicts: parking_lot::Mutex::new(verdicts.into_iter().map(Ok).collect()),
            allow_when_done: true,
            ..Self::default()
        })
    }

    /// `(tool, args, transcript)` of every review.
    pub fn seen(&self) -> Vec<(String, serde_json::Value, String)> {
        self.seen.lock().clone()
    }

    /// The transcript of every review.
    pub fn transcripts(&self) -> Vec<String> {
        self.seen().into_iter().map(|(_, _, t)| t).collect()
    }

    /// The tool of every review.
    pub fn tools(&self) -> Vec<String> {
        self.seen().into_iter().map(|(tool, _, _)| tool).collect()
    }
}

#[async_trait::async_trait]
impl ActionClassifier for ScriptedClassifier {
    async fn classify(
        &self,
        input: ClassifierInput<'_>,
    ) -> Result<ClassifierVerdict, ClassifierError> {
        self.seen.lock().push((
            input.tool.to_string(),
            input.args.clone(),
            input.transcript.render(),
        ));
        let mut verdicts = self.verdicts.lock();
        if !verdicts.is_empty() {
            return verdicts.remove(0);
        }
        if self.allow_when_done {
            Ok(ClassifierVerdict::Allow {
                reason: "scripted allow".into(),
            })
        } else {
            Err(ClassifierError::Provider("script exhausted".into()))
        }
    }
}
