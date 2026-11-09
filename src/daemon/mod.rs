//! `rkat-tui-mob`: the workspace's mob daemon (experimental, P1 of
//! `docs/mob-split.md`).
//!
//! The daemon embeds the engine the same way the in-process TUI does
//! ([`crate::bootstrap::assemble`]: realm, config layers, endpoints,
//! confinement plan, project trust, approval engine, instructions, MCP set
//! and factory), but composes Meerkat's JSON-RPC host on top of it instead
//! of a TUI session: a `meerkat_rpc` [`SessionRuntime`] with this crate's own
//! [`MobMcpState`] ([`engine`]), one durable host-root mob member as the
//! orchestrator ([`orchestrator`]), and the engine's `MethodRouter` served
//! per connection over loopback TCP ([`serve`]), each connection first
//! proving it holds the daemon's token ([`handshake`]). Clients come and
//! go; the runtime, the mob and its members stay.
//!
//! Where to connect: the endpoint file
//! `<state>/rkat-tui/<workspace key>/mob.endpoint` ([`endpoint`], in the
//! user's state directory, never in the workspace; mode 0600, removed on
//! shutdown) holds the bound address, the per-daemon token and the pid. The
//! token is the whole access control, so it lives where the agent's
//! confined shell cannot read it.
//!
//! Approvals (P3, P3b): every member's tool calls pass the TUI's approval
//! engine ([`approvals`]); a call that needs the human is held as a durable
//! decision, which the daemon pushes to every attached client
//! (`decision/notice`) and decides from a client's `decision/decide` as its
//! one configured principal ([`decisions`]); the permission mode and the
//! rules are reached through the daemon's own `rkat/*` methods ([`local`]);
//! the deny rules are the members' durable application tool policy
//! ([`policy`]).

pub mod approvals;
pub mod conversations;
pub mod decisions;
pub mod endpoint;
pub mod engine;
pub mod handshake;
pub mod lifecycle;
pub mod local;
pub mod orchestrator;
pub mod policy;
pub mod protocol;
pub mod serve;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use meerkat_core::ConfigStore;
use meerkat_mob_mcp::MobMcpState;
use meerkat_rpc::session_runtime::SessionRuntime;
use meerkat_store::RealmLeaseGuard;

use crate::bootstrap::{self, BootstrapOptions};
use crate::cli::Args;
use crate::error::TuiError;

pub use orchestrator::Orchestrator;

/// The surface name the daemon's realm lease records.
pub const LEASE_SURFACE: &str = "rkat-tui-mob";

/// The mob the daemon hosts in its realm (one per workspace realm).
pub const MOB_ID: &str = "tui";

/// The orchestrator's member identity (and its profile name).
pub const ORCHESTRATOR: &str = "main";

/// The endpoint file name (in the workspace's directory under the user's
/// state directory, see [`endpoint::default_file`]).
pub const ENDPOINT_NAME: &str = "mob.endpoint";

/// The default listen address: loopback, an ephemeral port.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:0";

/// Command-line arguments of `rkat-tui-mob`: its own flags, then every
/// `rkat-tui` flag that shapes the engine (realm, model, endpoints,
/// confinement, trust, `--set`). The UI-only ones (`--resume`, `-c`, a
/// prompt, mouse) are refused.
#[derive(Parser, Debug, Clone)]
#[command(name = "rkat-tui-mob", version)]
#[command(
    about = "Experimental: the rkat-tui mob daemon. Serves Meerkat's JSON-RPC methods for one workspace over loopback TCP"
)]
pub struct DaemonCli {
    /// Loopback address to listen on (port 0: an ephemeral port). The
    /// bound address and the token clients must present are written to
    /// the endpoint file (see `--endpoint-file`).
    #[arg(long, value_name = "ADDR", default_value = DEFAULT_LISTEN)]
    pub listen: std::net::SocketAddr,

    /// Where to write the endpoint file (default:
    /// `$XDG_STATE_HOME/rkat-tui/<workspace key>/mob.endpoint`, else under
    /// `~/.local/state`). It holds the attach token: the daemon refuses a
    /// path the agent's confined shell could read.
    #[arg(long, value_name = "PATH")]
    pub endpoint_file: Option<PathBuf>,

    /// Stay in the foreground (the only mode in this version).
    #[arg(long)]
    pub foreground: bool,

    /// The engine flags shared with `rkat-tui`.
    #[command(flatten)]
    pub tui: Args,
}

impl DaemonCli {
    /// Refuse the `rkat-tui` flags that mean nothing to a daemon.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Unsupported`] naming the first such flag.
    pub fn check(&self) -> Result<(), DaemonError> {
        let tui = &self.tui;
        let refused = [
            (tui.prompt.is_some(), "a prompt argument"),
            (tui.resume.is_some(), "--resume"),
            (tui.continue_last, "--continue"),
            (tui.attach_mode().is_some(), "--attach"),
        ];
        match refused.into_iter().find(|(given, _)| *given) {
            Some((_, flag)) => Err(DaemonError::Unsupported(flag)),
            None => Ok(()),
        }
    }
}

/// Why the daemon cannot start or serve.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// The shared bootstrap failed (realm, config, endpoints, confinement,
    /// trust, approval rules, MCP).
    #[error(transparent)]
    Bootstrap(Box<TuiError>),
    /// The realm declares a hosting mode its stores cannot provide.
    #[error("{0}")]
    Hosting(#[from] meerkat::HostingUnavailable),
    /// The approval side cannot be composed.
    #[error("cannot compose the approval gate: {0}")]
    Approval(String),
    /// The runtime refused the composition.
    #[error("cannot compose the session runtime: {0}")]
    Composition(#[from] meerkat::surface::RuntimeBackedCompositionError),
    /// The mob host could not attach to the runtime.
    #[error("cannot attach the mob host: {0}")]
    Driver(#[from] meerkat_runtime::RuntimeDriverError),
    /// Creating, restoring or spawning into the mob failed.
    #[error("mob {mob}: {source}")]
    Mob {
        /// The mob.
        mob: String,
        /// The engine's error.
        #[source]
        source: Box<meerkat_mob::MobError>,
    },
    /// The orchestrator has no session after its spawn or restore.
    #[error("the orchestrator `{0}` has no session")]
    NoSession(String),
    /// A daemon already answers at the address its endpoint file names.
    #[error(
        "a daemon already serves this workspace at {address} (pid {pid}, endpoint file {}); `/daemon stop` from a client attached to it, or SIGTERM to pid {pid}, ends it",
        file.display()
    )]
    AlreadyRunning {
        /// The live daemon's address.
        address: std::net::SocketAddr,
        /// Its process id.
        pid: u32,
        /// Its endpoint file.
        file: PathBuf,
    },
    /// The endpoint file would be readable by the agent's confined shell.
    #[error(
        "refusing to write the endpoint file {}: the agent's confined shell is granted {} and could read the attach token; choose another place with --endpoint-file or XDG_STATE_HOME",
        file.display(),
        grant.display()
    )]
    EndpointExposed {
        /// The endpoint file.
        file: PathBuf,
        /// The shell's grant that contains it.
        grant: PathBuf,
    },
    /// The listen address is not a loopback address.
    #[error("refusing to listen on {0}: only loopback addresses are served")]
    NotLoopback(std::net::SocketAddr),
    /// No random token could be drawn.
    #[error("cannot draw the daemon token: {0}")]
    Token(#[source] std::io::Error),
    /// A filesystem or socket operation failed.
    #[error("{context} {}: {source}", path.display())]
    Io {
        /// What was being attempted.
        context: &'static str,
        /// On which path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A flag only the interactive TUI understands.
    #[error("{0} is not supported by rkat-tui-mob")]
    Unsupported(&'static str),
    /// The runtime did not shut down cleanly.
    #[error("session runtime shutdown was incomplete: {0}")]
    Shutdown(#[from] meerkat_core::SessionError),
}

impl From<TuiError> for DaemonError {
    fn from(err: TuiError) -> Self {
        Self::Bootstrap(Box::new(err))
    }
}

impl From<DaemonError> for TuiError {
    fn from(err: DaemonError) -> Self {
        Self::Daemon(Box::new(err))
    }
}

impl DaemonError {
    pub(crate) fn mob(mob: &meerkat_mob::MobId, source: meerkat_mob::MobError) -> Self {
        Self::Mob {
            mob: mob.to_string(),
            source: Box::new(source),
        }
    }

    pub(crate) fn io(context: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            context,
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Engine overrides for [`Daemon::start`] (tests inject a scripted client
/// through [`DaemonOptions::with_bootstrap`]).
#[derive(Clone)]
pub struct DaemonOptions {
    /// The loopback address to listen on (default [`DEFAULT_LISTEN`]).
    pub listen: std::net::SocketAddr,
    /// Where to write the endpoint file (default:
    /// [`endpoint::default_file`] of the workspace).
    pub endpoint_file: Option<PathBuf>,
    /// The start token the launching client handed the daemon (in
    /// [`lifecycle::START_TOKEN_ENV`]): the connection presenting it is
    /// the owner.
    pub start_token: Option<String>,
    pub(crate) bootstrap: BootstrapOptions,
    pub(crate) policy: engine::PolicyOptions,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            listen: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            endpoint_file: None,
            start_token: None,
            bootstrap: BootstrapOptions::default(),
            policy: engine::PolicyOptions::default(),
        }
    }
}

impl DaemonOptions {
    /// Write the endpoint file at `file` instead of the default place.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_endpoint_file(mut self, file: PathBuf) -> Self {
        self.endpoint_file = Some(file);
        self
    }

    /// Started by a client that presents `token`.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_start_token(mut self, token: &str) -> Self {
        self.start_token = Some(token.to_string());
        self
    }

    /// Bootstrap with `bootstrap` (a scripted LLM, host paths, a lister).
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn with_bootstrap(mut self, bootstrap: BootstrapOptions) -> Self {
        self.bootstrap = bootstrap;
        self
    }

    /// Compose without the consequence-policy registry (to observe what
    /// the engine does with a member whose durable binding names it).
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn without_policy_registry(mut self) -> Self {
        self.policy.without_registry = true;
        self
    }
}

/// The endpoint file of the workspace at `context_root`, in the user's
/// state directory ([`endpoint::default_file`]).
///
/// # Errors
///
/// [`DaemonError::Io`] when there is no state directory.
pub fn endpoint_file(context_root: &Path) -> Result<PathBuf, DaemonError> {
    endpoint::default_file(context_root)
        .map_err(|err| DaemonError::io("find the endpoint file of", context_root, err))
}

/// Refuse an endpoint file the agent's confined shell could read: create
/// its directory (0700) and check the canonical directory against every
/// grant of the shell's `plan`.
///
/// # Errors
///
/// [`DaemonError::EndpointExposed`], or [`DaemonError::Io`] when the
/// directory cannot be created or resolved.
pub fn check_endpoint_private(
    plan: &bootstrap::ConfinementPlan,
    file: &Path,
) -> Result<(), DaemonError> {
    let dir = file.parent().unwrap_or_else(|| Path::new("."));
    endpoint::create_private_dir(dir)?;
    let canonical = std::fs::canonicalize(dir)
        .map_err(|err| DaemonError::io("resolve endpoint directory", dir, err))?;
    match endpoint::exposing_grant(plan, &canonical) {
        Some(grant) => Err(DaemonError::EndpointExposed {
            file: file.to_path_buf(),
            grant,
        }),
        None => Ok(()),
    }
}

/// A started daemon: the runtime and its mob host are composed, the
/// orchestrator is seated and the socket is bound; [`Daemon::serve_until`]
/// serves it.
pub struct Daemon {
    runtime: Arc<SessionRuntime>,
    config_store: Arc<dyn ConfigStore>,
    state: Arc<MobMcpState>,
    orchestrator: Orchestrator,
    lease: Option<RealmLeaseGuard>,
    listener: serve::Listener,
    #[cfg(any(test, feature = "test-support"))]
    scope: bootstrap::RuntimeScope,
    #[cfg(any(test, feature = "test-support"))]
    desk: Arc<approvals::Desk>,
    #[cfg(any(test, feature = "test-support"))]
    origins: Arc<approvals::DaemonToolOrigins>,
    local: Arc<local::Local>,
    /// The desk and the event watcher, aborted at shutdown.
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("address", &self.listener.address())
            .field("orchestrator", &self.orchestrator)
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// Bootstrap the workspace realm `args` names, compose the runtime and
    /// the mob host, create or restore the mob and its orchestrator, and
    /// bind the socket.
    ///
    /// # Errors
    ///
    /// [`DaemonError`] for every step.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn start(args: &Args, options: DaemonOptions) -> Result<Self, DaemonError> {
        let (layers, config) = bootstrap::prepare(args).await?;
        Self::start_with(layers, config, args, options).await
    }

    /// [`Daemon::start`] on already read layers and config.
    ///
    /// # Errors
    ///
    /// See [`Daemon::start`].
    pub async fn start_with(
        layers: bootstrap::Layers,
        config: meerkat_core::Config,
        args: &Args,
        options: DaemonOptions,
    ) -> Result<Self, DaemonError> {
        if !options.listen.ip().is_loopback() {
            return Err(DaemonError::NotLoopback(options.listen));
        }
        let started_at = endpoint::timestamp(std::time::SystemTime::now());
        let workspace = std::fs::canonicalize(&layers.scope.context_root)
            .unwrap_or_else(|_| layers.scope.context_root.clone());
        let file = match options.endpoint_file {
            Some(file) => file,
            None => endpoint_file(&layers.scope.context_root)?,
        };
        // Refuse before taking the realm: a live daemon keeps its endpoint.
        endpoint::check_stale(&file).await?;
        let parts =
            bootstrap::assemble(layers, config, args, options.bootstrap, LEASE_SURFACE).await?;
        check_endpoint_private(&parts.confinement, &file)?;
        let composed = engine::compose(parts, options.policy)?;
        let orchestrator = orchestrator::ensure(&composed.state, &composed.root).await?;
        // The orchestrator is idle now: its tool catalog can be read.
        if !composed.origins.refresh(&orchestrator.session_id).await {
            tracing::info!(
                "the orchestrator's tool catalog is read after its first run; until then only the TUI's own tools are known to the gate"
            );
        }
        let conversations = Arc::new(conversations::Conversations::open(
            Arc::clone(&composed.state),
            Arc::clone(&composed.root.registry),
            &composed.scope.context_root,
            &orchestrator.session_id,
            composed.root.model.clone(),
            composed.root.auth_binding.clone(),
        ));
        let watcher = approvals::watch(
            Arc::clone(&composed.state),
            orchestrator.mob_id.clone(),
            orchestrator.identity.clone(),
            Arc::clone(&composed.engine),
            Arc::clone(&composed.origins),
            Arc::clone(&conversations),
        );
        let local = Arc::new(local::Local::new(local::LocalParts {
            engine: Arc::clone(&composed.engine),
            state: Arc::clone(&composed.state),
            conversations,
            registry: Arc::clone(&composed.root.registry),
            origins: Arc::clone(&composed.origins),
            desk: Arc::clone(&composed.desk),
            thinking: composed.thinking.clone(),
            project: composed.project_inputs.clone(),
            decisions: Arc::clone(&composed.decisions),
            tasks_db: composed.tasks_db.clone(),
            machine: composed.runtime.runtime_adapter(),
            startup_notices: composed.startup_notices.clone(),
            lifecycle: lifecycle::Lifecycle::new(
                workspace.clone(),
                started_at.clone(),
                options.start_token,
            ),
        }));
        // The decisions an earlier daemon left open held dispatches that
        // ended with it.
        let leftovers = composed.decisions.withdraw_leftovers().await;
        if leftovers > 0 {
            tracing::info!(
                leftovers,
                "withdrew the approval requests an earlier daemon left open (their tool calls did not run)"
            );
        }
        if let Some(warning) = local.announce_plan_mode().await {
            tracing::warn!(%warning, "plan mode");
        }
        let listener =
            serve::Listener::bind(options.listen, &file, &workspace, &started_at).await?;
        tracing::info!(
            address = %listener.address(),
            endpoint = %file.display(),
            mob = MOB_ID,
            orchestrator = ORCHESTRATOR,
            session = %orchestrator.session_id,
            "rkat-tui-mob serving"
        );
        Ok(Self {
            runtime: composed.runtime,
            config_store: composed.config_store,
            state: composed.state,
            orchestrator,
            lease: Some(composed.lease),
            listener,
            #[cfg(any(test, feature = "test-support"))]
            scope: composed.scope,
            #[cfg(any(test, feature = "test-support"))]
            desk: composed.desk,
            #[cfg(any(test, feature = "test-support"))]
            origins: Arc::clone(&composed.origins),
            local,
            tasks: vec![
                composed.desk_task,
                composed.decisions_task,
                composed.diffs_task,
                composed.follower_task,
                composed.helper_catalogs_task,
                watcher,
            ],
        })
    }

    /// Where escalations wait for their decision.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn desk(&self) -> &Arc<approvals::Desk> {
        &self.desk
    }

    /// The members' tool catalogs the gate judges calls by.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn tool_origins(&self) -> &Arc<approvals::DaemonToolOrigins> {
        &self.origins
    }

    /// The bound address clients connect to.
    #[must_use]
    pub fn address(&self) -> std::net::SocketAddr {
        self.listener.address()
    }

    /// The endpoint file naming the address and the token.
    #[must_use]
    pub fn endpoint_file(&self) -> &Path {
        self.listener.endpoint_file()
    }

    /// The orchestrator member.
    #[must_use]
    pub fn orchestrator(&self) -> &Orchestrator {
        &self.orchestrator
    }

    /// The JSON-RPC session runtime.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn runtime(&self) -> &Arc<SessionRuntime> {
        &self.runtime
    }

    /// The realm and workspace the daemon serves.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn scope(&self) -> &bootstrap::RuntimeScope {
        &self.scope
    }

    /// Serve clients until `shutdown` resolves or a client asks for it
    /// (`rkat/shutdown`), then stop accepting, let the connections drain
    /// (bounded), shut the runtime down, drop the realm lease and remove
    /// the endpoint file.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Io`] when accepting fails, [`DaemonError::Shutdown`]
    /// when the runtime does not shut down cleanly.
    pub async fn serve_until(
        mut self,
        shutdown: impl std::future::Future<Output = ()> + Send,
    ) -> Result<(), DaemonError> {
        let lifecycle = Arc::clone(self.local.lifecycle());
        let shutdown = async move {
            tokio::select! {
                () = shutdown => {}
                () = lifecycle.shutdown_requested() => {}
            }
            // Connections ending from now on are no detach.
            lifecycle.terminating();
        };
        let served = serve::serve(
            &mut self.listener,
            &self.runtime,
            &self.config_store,
            &self.local,
            shutdown,
        )
        .await;
        // The endpoint goes first: nobody should find a daemon that no
        // longer accepts.
        self.listener.remove_files();
        let (served, mut connections) = match served {
            Ok(connections) => (Ok(()), connections),
            Err(err) => (Err(err), tokio::task::JoinSet::new()),
        };
        for task in self.tasks.drain(..) {
            task.abort();
        }
        stop_mobs(&self.state).await;
        let shutdown = self.runtime.try_shutdown().await;
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        if let Some(lease) = self.lease.take() {
            lease.shutdown().await;
        }
        served?;
        shutdown.map_err(DaemonError::from)
    }
}

/// How long the managed mobs get to shut down at exit.
const MOB_SHUTDOWN: std::time::Duration = std::time::Duration::from_secs(10);

/// Shut the managed mobs down, as the engine asks of a host before the
/// runtime's own shutdown (which stands the mob state down but "does not
/// shut down the state's managed mobs"): close admission, then shut each
/// mob in a snapshot down. Their members are not retired, so the next
/// start restores them; until a mob is shut down its comms names stay
/// taken in this process.
async fn stop_mobs(state: &Arc<MobMcpState>) {
    let deadline = std::time::Instant::now() + MOB_SHUTDOWN;
    if let meerkat_mob_mcp::MobStateStandDown::Unsettled { reasons } =
        state.close_admission(deadline).await
    {
        tracing::warn!(?reasons, "mob admission did not close cleanly");
    }
    let handles = match state.mob_handles_snapshot().await {
        Ok(handles) => handles,
        Err(err) => {
            tracing::warn!(error = %err, "cannot list the mobs to shut down");
            return;
        }
    };
    for (mob, handle) in handles {
        match tokio::time::timeout(MOB_SHUTDOWN, handle.shutdown()).await {
            Ok(Ok(())) => tracing::info!(%mob, "mob shut down"),
            Ok(Err(err)) => tracing::warn!(%mob, error = %err, "mob shutdown failed"),
            Err(_) => tracing::warn!(%mob, "mob did not shut down in time"),
        }
    }
}

/// Run the daemon in the foreground until SIGTERM or an interrupt.
///
/// # Errors
///
/// [`DaemonError`] when it cannot start or does not shut down cleanly.
pub async fn run(cli: DaemonCli) -> Result<(), DaemonError> {
    cli.check()?;
    let (layers, config) = bootstrap::prepare(&cli.tui).await?;
    init_logging(&layers.scope.realm_root());
    let daemon = Daemon::start_with(
        layers,
        config,
        &cli.tui,
        DaemonOptions {
            listen: cli.listen,
            endpoint_file: cli.endpoint_file.clone(),
            start_token: std::env::var(lifecycle::START_TOKEN_ENV).ok(),
            ..DaemonOptions::default()
        },
    )
    .await?;
    eprintln!(
        "rkat-tui-mob: serving {} (endpoint file {}; mob `{MOB_ID}`, orchestrator `{ORCHESTRATOR}`, session {})",
        daemon.address(),
        daemon.endpoint_file().display(),
        daemon.orchestrator().session_id
    );
    daemon.serve_until(termination_signal()).await
}

/// Resolves on SIGTERM or an interrupt (Ctrl+C).
async fn termination_signal() {
    let interrupt = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => tracing::info!("interrupted: shutting down"),
        () = terminate => tracing::info!("SIGTERM: shutting down"),
    }
}

/// Log to stderr and, like the TUI, to `<realm>/logs/rkat-tui-mob.log`
/// (the file is skipped, with a warning, when it cannot be opened).
fn init_logging(realm_root: &Path) {
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    };
    let log_dir = realm_root.join("logs");
    let file = std::fs::create_dir_all(&log_dir)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_dir.join("rkat-tui-mob.log"))
        })
        .map_err(|err| eprintln!("rkat-tui-mob: no log file in {}: {err}", log_dir.display()))
        .ok();
    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(filter());
    let file = file.map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .with_filter(filter())
    });
    let _ = tracing_subscriber::registry()
        .with(stderr)
        .with(file)
        .try_init();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_daemon_shares_the_tui_engine_flags_and_refuses_the_ui_ones() {
        let cli = DaemonCli::try_parse_from([
            "rkat-tui-mob",
            "-w",
            "/tmp/ws",
            "--listen",
            "127.0.0.1:7777",
            "--foreground",
            "--model",
            "claude-sonnet-4-6",
            "--no-confinement",
        ])
        .unwrap();
        assert_eq!(cli.listen.port(), 7777);
        assert_eq!(
            DaemonCli::try_parse_from(["rkat-tui-mob"]).unwrap().listen,
            DEFAULT_LISTEN.parse().unwrap()
        );
        assert!(cli.foreground);
        assert_eq!(cli.tui.model.as_deref(), Some("claude-sonnet-4-6"));
        assert!(cli.check().is_ok());
        for argv in [
            vec!["rkat-tui-mob", "hello"],
            vec!["rkat-tui-mob", "--resume", "last"],
            vec!["rkat-tui-mob", "-c"],
        ] {
            let cli = DaemonCli::try_parse_from(argv).unwrap();
            assert!(matches!(cli.check(), Err(DaemonError::Unsupported(_))));
        }
    }

    #[test]
    fn the_endpoint_file_lives_in_the_user_state_dir_keyed_by_workspace() {
        let state = Path::new("/state");
        let file = endpoint::file_in(state, Path::new("/w"));
        let key = endpoint::workspace_key(Path::new("/w"));
        assert_eq!(key.len(), endpoint::KEY_DIGITS);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(file, state.join("rkat-tui").join(&key).join(ENDPOINT_NAME));
        assert_ne!(key, endpoint::workspace_key(Path::new("/v")));
        let default = endpoint_file(Path::new("/w")).unwrap();
        assert!(!default.starts_with("/w"), "{}", default.display());
        assert!(default.ends_with(Path::new("rkat-tui").join(&key).join(ENDPOINT_NAME)));
    }

    /// The finding this guards: the token was in `<workspace>/.rkat/tui`,
    /// which the agent's confined shell can read.
    #[test]
    fn the_confined_shell_is_granted_nothing_that_holds_the_endpoint_file() {
        let workspace = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(workspace.path()).unwrap();
        let requirement = bootstrap::confinement::lower(
            crate::cli::ConfinementArg::Workspace,
            &workspace,
            &bootstrap::HostPaths::detect(),
        )
        .unwrap()
        .unwrap();
        let spec = requirement.specification();
        let meerkat_core::confinement::FilesystemAccess::Paths(granted) = &spec.read else {
            panic!("a workspace plan grants named paths");
        };
        let default = endpoint_file(&workspace).unwrap();
        let dir = default.parent().unwrap();
        // The state directory may not exist yet: resolve its deepest
        // existing ancestor, as the grants are canonical.
        let mut existing = dir.to_path_buf();
        while !existing.exists() {
            existing.pop();
        }
        let resolved = std::fs::canonicalize(&existing)
            .unwrap()
            .join(dir.strip_prefix(&existing).unwrap());
        for grant in granted {
            assert!(
                !resolved.starts_with(grant.path()) && !dir.starts_with(grant.path()),
                "{} is inside the grant {}",
                dir.display(),
                grant.path().display()
            );
        }
        let plan = bootstrap::ConfinementPlan {
            mode: crate::cli::ConfinementArg::Workspace,
            shell: meerkat_tools::builtin::shell::ShellConfinement::Required { requirement },
            notices: Vec::new(),
            shell_env: None,
        };
        assert_eq!(endpoint::exposing_grant(&plan, &resolved), None);
        // The old place, and any place in the workspace, is refused (the
        // grant is the workspace, or the temp dir that holds it).
        let old = workspace.join(".rkat").join("tui").join(ENDPOINT_NAME);
        assert!(matches!(
            check_endpoint_private(&plan, &old),
            Err(DaemonError::EndpointExposed { grant, .. }) if workspace.starts_with(&grant)
        ));
        // An unconfined shell is the trusted host: nothing to keep from it.
        let off = bootstrap::ConfinementPlan {
            mode: crate::cli::ConfinementArg::Off,
            shell: meerkat_tools::builtin::shell::ShellConfinement::TrustedHost,
            notices: Vec::new(),
            shell_env: None,
        };
        check_endpoint_private(&off, &old).unwrap();
    }
}
