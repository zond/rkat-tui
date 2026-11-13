//! `rkat-tui-mob`: the workspace's mob daemon (`docs/mob-split.md`).
//!
//! The daemon embeds the engine through the shared bootstrap
//! ([`crate::bootstrap::assemble`]: realm, config layers, endpoints,
//! confinement plan, project trust, approval engine, instructions, MCP set
//! and factory) and composes Meerkat's JSON-RPC host on top of it: a `meerkat_rpc` [`SessionRuntime`] with this crate's own
//! [`MobMcpState`] ([`engine`]), one durable host-root mob member as the
//! agent ([`mob`]), and the engine's `MethodRouter` served
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
//! Approvals: every member's tool calls pass rkat-tui's approval
//! engine ([`approvals`]); a call that needs the human is held as a durable
//! decision, which the daemon pushes to every attached client
//! (`decision/notice`) and decides from a client's `decision/decide` as its
//! one configured principal ([`decisions`]); the permission mode and the
//! rules are reached through the daemon's own `rkat/*` methods ([`local`]);
//! the deny rules are the members' durable application tool policy
//! ([`policy`]).

pub mod activity;
pub mod agents;
pub mod approvals;
pub mod audit;
pub mod context;
pub mod conversations;
pub mod decisions;
pub mod deferral;
pub mod endpoint;
pub mod engine;
pub mod handshake;
pub mod lifecycle;
pub mod local;
pub mod mob;
pub mod policy;
pub mod protocol;
pub mod schedules;
pub mod schema_compat;
pub mod serve;
pub mod tasks;
pub mod usage;
pub mod watches;

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

pub use mob::FirstAgent;

/// The surface name the daemon's realm lease records.
pub const LEASE_SURFACE: &str = "rkat-tui-mob";

/// The mob the daemon hosts in its realm (one per workspace realm).
pub const MOB_ID: &str = "tui";

/// The agents' profile name (agents are named `main-<id>`, see
/// [`agents`]).
pub const AGENT_PROFILE: &str = "main";

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
#[command(name = "rkat-tui-mob", version = crate::BUILD)]
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
            (tui.attach.is_some(), "--attach"),
            (tui.attach_token_file.is_some(), "--attach-token-file"),
            (tui.attach_insecure, "--attach-insecure"),
            (tui.rehome.is_some(), "--rehome"),
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
    /// The conversation log cannot be read.
    #[error(transparent)]
    ConversationLog(#[from] conversations::LogError),
    /// The workspace's first agent could not be seated.
    #[error("cannot seat the workspace's first agent: {0}")]
    FirstAgent(String),
    /// The agent has no session after its spawn or restore.
    #[error("the agents' profile `{0}` has no session")]
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
    /// An endpoint file names a daemon nobody can say is gone (its
    /// address did not answer in time, or the file cannot be read).
    #[error(
        "cannot tell whether a daemon still serves this workspace (endpoint file {}: {why}); if none does, remove the file and start again",
        file.display()
    )]
    EndpointUndecided {
        /// The endpoint file.
        file: PathBuf,
        /// What the check found.
        why: String,
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
    /// The build the daemon names (`None`: [`crate::BUILD`]).
    pub(crate) build: Option<String>,
    /// Tests: how many of the next agent spawns fail.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) spawn_faults: Option<Arc<std::sync::atomic::AtomicUsize>>,
    /// The state directory the default endpoint file goes in (`None`:
    /// the user's, [`endpoint::state_home`], or the runtime directory's).
    pub(crate) state_home: Option<PathBuf>,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            listen: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            endpoint_file: None,
            start_token: None,
            bootstrap: BootstrapOptions::default(),
            policy: engine::PolicyOptions::default(),
            build: None,
            state_home: None,
            #[cfg(any(test, feature = "test-support"))]
            spawn_faults: None,
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

    /// Put the default endpoint file under `dir` instead of the user's
    /// state directory: the default place is still chosen (and checked
    /// against the shell's grants), in a directory a test owns.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_state_home(mut self, dir: PathBuf) -> Self {
        self.state_home = Some(dir);
        self
    }

    /// Fail as many of the next agent spawns as `faults` holds (a switch,
    /// and seating the previous conversation again, can then fail).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_spawn_faults(mut self, faults: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        self.spawn_faults = Some(faults);
        self
    }

    /// Name `build` as the daemon's build (a client of another build
    /// says so).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_build(mut self, build: &str) -> Self {
        self.build = Some(build.to_string());
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

/// The first of `files` the agent's confined shell cannot read (see
/// [`check_endpoint_private`]): the state directory's, else the runtime
/// directory's fallback.
///
/// # Errors
///
/// The first file's refusal ([`DaemonError::EndpointExposed`] with today's
/// message) when none is safe.
pub fn choose_endpoint_file(
    plan: &bootstrap::ConfinementPlan,
    files: &[PathBuf],
) -> Result<PathBuf, DaemonError> {
    let mut first_refusal = None;
    for file in files {
        match check_endpoint_private(plan, file) {
            Ok(()) => {
                if first_refusal.is_some() {
                    tracing::info!(file = %file.display(), "the state directory is granted to the agent's shell; the endpoint file goes to the runtime directory");
                }
                return Ok(file.clone());
            }
            Err(err) => {
                first_refusal.get_or_insert(err);
            }
        }
    }
    Err(first_refusal.unwrap_or_else(|| {
        DaemonError::io(
            "find the endpoint file",
            Path::new(""),
            std::io::Error::new(std::io::ErrorKind::NotFound, "no place for it"),
        )
    }))
}

/// A started daemon: the runtime and its mob host are composed, the
/// agent is seated and the socket is bound; [`Daemon::serve_until`]
/// serves it.
pub struct Daemon {
    runtime: Arc<SessionRuntime>,
    config_store: Arc<dyn ConfigStore>,
    state: Arc<MobMcpState>,
    first_agent: FirstAgent,
    lease: Option<RealmLeaseGuard>,
    listener: serve::Listener,
    /// The realm's directory (its databases settle at shutdown).
    realm_root: PathBuf,
    #[cfg(any(test, feature = "test-support"))]
    scope: bootstrap::RuntimeScope,
    #[cfg(any(test, feature = "test-support"))]
    desk: Arc<approvals::Desk>,
    #[cfg(any(test, feature = "test-support"))]
    origins: Arc<approvals::DaemonToolOrigins>,
    #[cfg(any(test, feature = "test-support"))]
    registry: Arc<crate::mobs::HelperRegistry>,
    #[cfg(any(test, feature = "test-support"))]
    engine: Arc<crate::approval::ApprovalEngine>,
    #[cfg(any(test, feature = "test-support"))]
    compact_threshold: Option<std::num::NonZeroU64>,
    local: Arc<local::Local>,
    /// The desk and the event watcher, aborted at shutdown.
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("address", &self.listener.address())
            .field("first_agent", &self.first_agent)
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// Bootstrap the workspace realm `args` names, compose the runtime and
    /// the mob host, create or restore the mob and its agent, and
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
        mut layers: bootstrap::Layers,
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
        let files = endpoint_files(
            options.endpoint_file,
            options.state_home,
            &layers.scope.context_root,
        )?;
        // Refuse before taking the realm unless every place is free or
        // proven stale (a refused connect): a live daemon keeps its
        // endpoint, at either place, and a slow one must not be mistaken
        // for a dead one.
        for file in &files {
            endpoint::check_stale(file).await?;
        }
        // The token's directories are secret to the agent's file tools
        // (read included) and to the classifier's: the confined shell
        // cannot reach them ([`choose_endpoint_file`]), and neither may a
        // `read_file` a classifier would allow.
        let secret: Vec<PathBuf> = endpoint::search_dirs()
            .into_iter()
            .chain(
                files
                    .iter()
                    .filter_map(|file| file.parent().map(Path::to_path_buf)),
            )
            .collect();
        layers.scope.project_root = layers.scope.project_root.clone().with_secret_roots(secret);
        let parts =
            bootstrap::assemble(layers, config, args, options.bootstrap, LEASE_SURFACE).await?;
        let file = choose_endpoint_file(&parts.confinement, &files)?;
        let mut composed = engine::compose(parts, options.policy)?;
        // The audit log in the realm: owner-only, outside the workspace
        // and every grant of the agent's shell, secret to its file tools
        // (see `audit`); it follows a re-home.
        let audit = match audit::AuditLog::open(&composed.scope.realm_root()) {
            Ok(log) => Some(log),
            Err(err) => {
                tracing::warn!(error = %err, "no audit log this run");
                composed
                    .startup_notices
                    .push(protocol::StartupNotice::warn(format!(
                        "no audit log this run: {err}"
                    )));
                None
            }
        };
        mob::ensure_mob(&composed.state, &composed.root).await?;
        let conversations = conversations::Conversations::open(
            Arc::clone(&composed.state),
            Arc::clone(&composed.root.registry),
            &composed.scope.realm_root(),
            composed.root.model.clone(),
            composed.root.auth_binding.clone(),
        )?
        .with_deferral(Arc::clone(&composed.deferral));
        #[cfg(any(test, feature = "test-support"))]
        let conversations = conversations.with_spawn_faults(options.spawn_faults.clone());
        let conversations = Arc::new(conversations);
        let lifecycle = lifecycle::Lifecycle::built(
            workspace.clone(),
            started_at.clone(),
            options.start_token,
            options.build.unwrap_or_else(|| crate::BUILD.to_string()),
        );
        let activity = idle_activity(
            &conversations,
            composed.runtime.runtime_adapter(),
            &composed.decisions,
        );
        let agents = agents::Agents::new(agents::AgentsParts {
            activity,
            state: Arc::clone(&composed.state),
            conversations: Arc::clone(&conversations),
            engine: Arc::clone(&composed.engine),
            origins: Arc::clone(&composed.origins),
            lifecycle: Arc::clone(&lifecycle),
            schedules: Arc::new(schedules::Schedules::new(
                composed.runtime.schedule_service(),
                Arc::clone(&composed.ledger),
            )),
            decisions: Arc::clone(&composed.decisions),
            audit: audit.clone(),
            watches: Arc::clone(&composed.watches),
            ledger: Arc::clone(&composed.ledger),
        });
        attach_engine(&composed.engine, &agents, audit.as_ref(), &workspace)?;
        composed.desk.report_to(&composed.engine);
        // Restored (their classifier seeded with what the user asked so
        // far), else one seated for the first client.
        let (first, session, created) = agents.start().await?;
        let watch_task = deliver_watches(
            &agents,
            std::mem::replace(
                &mut composed.watch_batches,
                tokio::sync::mpsc::unbounded_channel().1,
            ),
        );
        // Schedules fire while the daemon runs, from its start: the
        // runtime arms its firing host on a session's creation only, and a
        // restored agent creates none.
        if let Err(err) = composed.runtime.ensure_schedule_host_started().await {
            tracing::warn!(error = %err, "the schedule host did not start; schedules fire once a session is created");
        }
        // Records of schedules deleted while the daemon was down go.
        agents.schedules().reconcile().await;
        let first_agent = FirstAgent {
            mob_id: meerkat_mob::MobId::from(MOB_ID),
            identity: meerkat_mob::AgentIdentity::from(first.as_str()),
            session_id: session,
            created,
        };
        // The agents are idle now: their tool catalogs can be read.
        for (agent, session) in agents.live() {
            if !composed.origins.refresh(&session).await {
                tracing::info!(
                    %agent,
                    "the agent's tool catalog is read after its first run; until then only rkat-tui's own tools are known to the gate"
                );
            }
        }
        let local = Arc::new(local::Local::new(local::LocalParts {
            engine: Arc::clone(&composed.engine),
            state: Arc::clone(&composed.state),
            conversations,
            agents: Arc::clone(&agents),
            registry: Arc::clone(&composed.root.registry),
            origins: Arc::clone(&composed.origins),
            deferral: Arc::clone(&composed.deferral),
            desk: Arc::clone(&composed.desk),
            thinking: composed.thinking.clone(),
            project: composed.project_inputs.clone(),
            decisions: Arc::clone(&composed.decisions),
            tasks_db: composed.tasks_db.clone(),
            realm_file: composed.realm_file.clone(),
            machine: composed.runtime.runtime_adapter(),
            realm_config: Arc::clone(&composed.realm_config),
            skills: Arc::clone(&composed.skills),
            workspace: composed.scope.context_root.clone(),
            endpoints: composed.endpoints.clone(),
            startup_notices: composed.startup_notices.clone(),
            lifecycle,
            audit,
            report: local::ReportParts {
                realm: composed.scope.realm_root(),
                realm_id: composed.scope.realm_id.clone(),
                confinement: clap::ValueEnum::to_possible_value(&args.effective_confinement())
                    .map_or_else(
                        || format!("{:?}", args.effective_confinement()),
                        |value| value.get_name().to_string(),
                    ),
                helper_depth: composed.helper_max_depth,
            },
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
            agent = %first_agent.identity,
            session = %first_agent.session_id,
            "rkat-tui-mob serving"
        );
        Ok(Self {
            runtime: composed.runtime,
            config_store: composed.config_store,
            state: composed.state,
            first_agent,
            lease: Some(composed.lease),
            listener,
            realm_root: composed.scope.realm_root(),
            #[cfg(any(test, feature = "test-support"))]
            scope: composed.scope,
            #[cfg(any(test, feature = "test-support"))]
            desk: composed.desk,
            #[cfg(any(test, feature = "test-support"))]
            origins: Arc::clone(&composed.origins),
            #[cfg(any(test, feature = "test-support"))]
            registry: Arc::clone(&composed.root.registry),
            #[cfg(any(test, feature = "test-support"))]
            engine: Arc::clone(&composed.engine),
            #[cfg(any(test, feature = "test-support"))]
            compact_threshold: composed.root.compact_threshold,
            local,
            tasks: vec![
                watch_task,
                composed.desk_task,
                composed.decisions_task,
                composed.diffs_task,
                composed.notes_task,
                composed.follower_task,
                composed.destroyed_task,
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

    /// The daemon's own methods and its view of the proxied ones (tests).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn local(&self) -> &Arc<local::Local> {
        &self.local
    }

    /// The engine's own method router, past the proxy (tests: what a
    /// plain session in the realm looks like, which no client can create).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn engine_side(&self) -> testing::EngineSide {
        testing::EngineSide {
            runtime: Arc::downgrade(&self.runtime),
            config_store: Arc::downgrade(&self.config_store),
            deferral: Arc::downgrade(self.local.deferral()),
        }
    }

    /// The agent profile's auto-compaction threshold.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn compact_threshold(&self) -> Option<std::num::NonZeroU64> {
        self.compact_threshold
    }

    /// The mob host every member lives in.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn mob_state(&self) -> &Arc<MobMcpState> {
        &self.state
    }

    /// The helpers' bookkeeping (owners, running set, durable bindings).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn helper_registry(&self) -> &Arc<crate::mobs::HelperRegistry> {
        &self.registry
    }

    /// The approval engine every member's gate shares.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn approval_engine(&self) -> &Arc<crate::approval::ApprovalEngine> {
        &self.engine
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

    /// The agent the daemon started with (the most recent one it
    /// restored, else the one it seated for the first client).
    #[must_use]
    pub fn first_agent(&self) -> &FirstAgent {
        &self.first_agent
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
        // "Terminate": the agents leave with the daemon (their
        // conversations stay); else they are restored at the next start.
        if self.local.lifecycle().retires_all() {
            self.local.retire_all().await;
        }
        // The conversation log as it is now; a write still on its way is
        // older and dropped.
        self.local.flush_conversations();
        self.local.audit(audit::Line::event(
            "daemon_stop",
            format!("pid {}", std::process::id()),
        ));
        let mut connections = served;
        for task in &self.tasks {
            task.abort();
        }
        // After the watchers counted every model call their streams hold.
        self.local.agents().stop_watching().await;
        // Joined, so nothing they hold outlives the shutdown.
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
        // The usage counted since the last run end, off the loop.
        let ledger = Arc::clone(self.local.agents().ledger());
        let _ = tokio::task::spawn_blocking(move || ledger.flush()).await;
        stop_mobs(&self.state).await;
        let shutdown = self.runtime.try_shutdown().await;
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        if let Some(lease) = self.lease.take() {
            lease.shutdown().await;
        }
        // The engine's stores close some connections only at their idle
        // bound, after the last handle is gone (the schedule store's worker
        // after 250 ms): the realm is settled when the last connection of
        // every database has closed, which removes its write-ahead log.
        let realm_root = self.realm_root.clone();
        drop(self);
        settle_stores(&realm_root, STORE_SETTLE).await;
        shutdown.map_err(DaemonError::from)
    }
}

/// Test seams of the daemon.
#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    use std::sync::{Arc, Weak};

    use meerkat_core::ConfigStore;
    use meerkat_rpc::session_runtime::SessionRuntime;
    use serde_json::Value;

    /// The engine's method router of a serving daemon, without the proxy's
    /// method allow-list ([`super::protocol::engine::SERVED`]). Holds the
    /// runtime weakly: it never keeps a stopped daemon's stores alive.
    #[derive(Clone)]
    pub struct EngineSide {
        pub(super) runtime: Weak<SessionRuntime>,
        pub(super) config_store: Weak<dyn ConfigStore>,
        pub(super) deferral: Weak<super::deferral::Deferral>,
    }

    impl EngineSide {
        /// Call `method` on the engine directly; its result, or its error
        /// as text.
        ///
        /// # Errors
        ///
        /// The engine's error, or why the call could not be made (the
        /// daemon stopped).
        pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            let (Some(runtime), Some(config_store), Some(deferral)) = (
                self.runtime.upgrade(),
                self.config_store.upgrade(),
                self.deferral.upgrade(),
            ) else {
                return Err("the daemon stopped".to_string());
            };
            let router = deferral
                .with_router(&runtime, || {
                    meerkat_rpc::router::MethodRouter::new(
                        Arc::clone(&runtime),
                        config_store,
                        meerkat_rpc::router::NotificationSink::noop(),
                    )
                })
                .await
                .map_err(|err| err.to_string())?;
            let request = serde_json::from_value(serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
            }))
            .map_err(|err| err.to_string())?;
            let response = router
                .dispatch(request)
                .await
                .ok_or_else(|| "no answer".to_string())?;
            let response = serde_json::to_value(&response).map_err(|err| err.to_string())?;
            match response.get("error") {
                Some(error) => Err(error.to_string()),
                None => Ok(response.get("result").cloned().unwrap_or(Value::Null)),
            }
        }
    }
}

/// How long a stopped daemon waits for the realm's databases to close.
const STORE_SETTLE: std::time::Duration = std::time::Duration::from_secs(3);

/// Wait, at most `bound`, until this process holds no SQLite database
/// file under `realm_root` open (the databases, their write-ahead logs and
/// shared memory), as `/proc/self/fd` shows. Past the bound the outcome is
/// logged as unknown; where the process's descriptors cannot be listed
/// (not Linux) nothing is waited for and nothing is claimed.
/// The endpoint files the daemon may write: the one named, the one in a
/// state home, else the workspace's candidates (the state directory's,
/// then the runtime directory's fallback).
fn endpoint_files(
    named: Option<PathBuf>,
    state_home: Option<PathBuf>,
    workspace: &Path,
) -> Result<Vec<PathBuf>, DaemonError> {
    Ok(match (named, state_home) {
        (Some(file), _) => vec![file],
        (None, Some(state_home)) => vec![endpoint::file_in(&state_home, workspace)],
        (None, None) => {
            let mut files = endpoint::candidates(workspace);
            if files.is_empty() {
                files.push(endpoint_file(workspace)?);
            }
            files
        }
    })
}

/// When the agents are idle ([`activity`]): the runtime is asked for a
/// conversation's unfinished inputs (one that cannot be read counts) and
/// the agent's run state; every client is told.
fn idle_activity(
    conversations: &Arc<conversations::Conversations>,
    machine: Arc<meerkat_runtime::MeerkatMachine>,
    decisions: &Arc<decisions::Decisions>,
) -> Arc<activity::Activity> {
    let probe = {
        let conversations = Arc::clone(conversations);
        Box::new(move |agent: String| {
            let conversations = Arc::clone(&conversations);
            let machine = Arc::clone(&machine);
            Box::pin(async move {
                use meerkat_runtime::SessionServiceRuntimeExt as _;
                let session = conversations.current(&agent)?;
                let unfinished = machine
                    .list_active_inputs(&session)
                    .await
                    .map_or(1, |inputs| inputs.len());
                let run = conversations.running(&agent).await;
                Some(activity::Probed {
                    session,
                    unfinished,
                    run,
                })
            }) as futures::future::BoxFuture<'static, Option<activity::Probed>>
        })
    };
    let decisions = Arc::clone(decisions);
    activity::Activity::new(
        probe,
        Box::new(move |note: &protocol::IdleNote| {
            decisions.broadcast(protocol::IDLE, note);
        }),
    )
}

/// What the approval engine learns of the agents before any runs: who is
/// with each (a turn no user started is unattended unless a client is),
/// which peers are the workspace's agents (a message to one is tier 1),
/// and the audit log. Each is installed once: a second composition over
/// one engine is refused rather than left on the first one's agents.
fn attach_engine(
    engine: &Arc<crate::approval::ApprovalEngine>,
    agents: &Arc<agents::Agents>,
    audit: Option<&Arc<audit::AuditLog>>,
    workspace: &Path,
) -> Result<(), DaemonError> {
    if !engine.set_attendance(Arc::new(agents::AgentAttendance::new(agents))) {
        return Err(DaemonError::Approval(
            "the approval engine already knows who is with its agents".to_string(),
        ));
    }
    if !engine.set_agent_peers(Arc::new(agents::AgentAttendance::new(agents))) {
        return Err(DaemonError::Approval(
            "the approval engine already knows the workspace's agents".to_string(),
        ));
    }
    if let Some(log) = audit {
        engine.set_audit(Arc::new(audit::DaemonAudit::new(Arc::clone(log), agents)));
        log.write(&audit::Line::event(
            "daemon_start",
            format!(
                "pid {} on {}, mode {}",
                std::process::id(),
                workspace.display(),
                engine.mode()
            ),
        ));
    }
    Ok(())
}

/// The agents' file watches: each batch reaches its agent.
fn deliver_watches(
    agents: &Arc<agents::Agents>,
    mut batches: tokio::sync::mpsc::UnboundedReceiver<watches::Batch>,
) -> tokio::task::JoinHandle<()> {
    let agents = Arc::downgrade(agents);
    tokio::spawn(async move {
        while let Some(batch) = batches.recv().await {
            let Some(agents) = agents.upgrade() else {
                return;
            };
            agents.watch_event(batch).await;
        }
    })
}

async fn settle_stores(realm_root: &Path, bound: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + bound;
    loop {
        let Some(open) = open_databases(realm_root) else {
            tracing::debug!(
                "the process's descriptors cannot be listed; the realm's databases are not waited for"
            );
            return;
        };
        if open.is_empty() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(?open, "the realm's databases may still be open at exit");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// The SQLite files under `realm_root` this process has open, or `None`
/// when its descriptors cannot be listed.
fn open_databases(realm_root: &Path) -> Option<Vec<PathBuf>> {
    let descriptors = std::fs::read_dir("/proc/self/fd").ok()?;
    let mut open: Vec<PathBuf> = descriptors
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .filter(|target| target.starts_with(realm_root) && is_database_file(target))
        .collect();
    open.sort();
    open.dedup();
    Some(open)
}

/// Whether `path` names a SQLite database or one of its companion files
/// that SQLite removes at the last close (the lock files beside them stay).
fn is_database_file(path: &Path) -> bool {
    let name = path.to_string_lossy();
    [".sqlite3", ".db", "-wal", "-shm", "-journal"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
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
        "rkat-tui-mob: serving {} (endpoint file {}; mob `{MOB_ID}`, agent `{}`, session {})",
        daemon.address(),
        daemon.endpoint_file().display(),
        daemon.first_agent().identity,
        daemon.first_agent().session_id
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

    /// A database file this process holds open is listed, its lock files
    /// are not, and the settle waits for the close (the negative case: an
    /// open file is never taken as closed).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_shutdown_waits_for_the_realms_open_databases() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let db = root.join("sessions.sqlite3");
        let lock = root.join("sessions.sqlite3.mfence");
        let held = std::fs::File::create(&db).unwrap();
        let _lock = std::fs::File::create(&lock).unwrap();
        assert_eq!(open_databases(&root), Some(vec![db.clone()]));
        let closer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            drop(held);
        });
        let started = std::time::Instant::now();
        settle_stores(&root, std::time::Duration::from_secs(10)).await;
        assert!(started.elapsed() >= std::time::Duration::from_millis(100));
        assert_eq!(open_databases(&root), Some(Vec::new()));
        closer.await.unwrap();
    }

    #[test]
    fn the_daemon_shares_the_tui_engine_flags_and_refuses_the_ui_ones() {
        let cli = DaemonCli::try_parse_from([
            "rkat-tui-mob",
            "-w",
            "/tmp/ws",
            "--listen",
            "127.0.0.1:7777",
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
        // `--foreground` was the only mode and is gone as a flag.
        assert!(DaemonCli::try_parse_from(["rkat-tui-mob", "--foreground"]).is_err());
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
            shell: meerkat_tools::builtin::shell::ShellConfinement::TrustedHost,
            notices: Vec::new(),
            shell_env: None,
        };
        check_endpoint_private(&off, &old).unwrap();
        // A refused place falls back to the next; with none safe, the
        // first refusal is the error.
        let outside = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let fallback = outside.path().join("rt").join(ENDPOINT_NAME);
        let exposed_plan = plan;
        let granted_fallback = endpoint::exposing_grant(
            &exposed_plan,
            &std::fs::canonicalize(outside.path()).unwrap(),
        );
        if granted_fallback.is_none() {
            assert_eq!(
                choose_endpoint_file(&exposed_plan, &[old.clone(), fallback.clone()]).unwrap(),
                fallback
            );
        } else {
            eprintln!("SKIPPED: the current directory is granted to the shell here");
        }
        assert!(matches!(
            choose_endpoint_file(&exposed_plan, std::slice::from_ref(&old)),
            Err(DaemonError::EndpointExposed { file, .. }) if file == old
        ));
    }
}
