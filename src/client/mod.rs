//! The client: `rkat-tui` runs the TUI against the workspace's
//! `rkat-tui-mob` daemon, which hosts the agent (the always-daemon plan in
//! `docs/mob-split.md`).
//!
//! - [`transport`]: the connection (attach handshake with the endpoint
//!   file's token, then a JSON-RPC multiplexer over newline-delimited JSON).
//! - [`launch`]: start-on-demand (probe the endpoint file, start the daemon
//!   in the background when none answers, wait for its endpoint).
//! - [`remote`]: the agent member through the engine's own methods
//!   (`mob/member_send`, `mob/stream_open`,
//!   `events/list_since`, `mob/stop_member_run`, `mob/force_cancel`).
//! - [`catchup`]: the stream cursor `{epoch, seq}` the client reattaches
//!   with, the once-only rule, and the per-workspace cursor file.
//! - [`approvals`]: the approval dialogs: the daemon's pushed decisions
//!   (`decision/notice`, `decision/decide`).
//! - [`runner`]: the loop between the UI and the daemon over the UI's
//!   [`Effect`]/[`Msg`] contract: approval dialogs, permission modes and `/permissions` act on the
//!   daemon's gate (`rkat/*` methods, see [`crate::daemon::local`]); the
//!   user's `!` shell, the model picker (this process's endpoints),
//!   `/remember`, `/memory`, `/export` and the cost readout run here, in
//!   the [`crate::workbench`] (the new context reaches the agent
//!   with `mob/append_system_context`);
//!   `/trust` acts on the daemon's trust record (`rkat/trust`); `/compact now` asks the daemon's
//!   engine (`mob/compact_member`).
//! - [`helpers`]: the follower view of the agent's helpers, from the
//!   mobs' own event streams.
//! - [`sessions`]: `/new`, `/resume` (the daemon's `rkat/new`,
//!   `rkat/resume`, `rkat/conversations`, `rkat/transcript`).
//!
//! What runs on this side of the wire is the [`crate::workbench`] and the
//! UI it starts ([`crate::workbench::app::build_app`]).

pub mod address;
pub mod approvals;
pub mod catchup;
pub mod helpers;
pub mod launch;
pub mod list;
pub mod push;
pub mod rehome;
pub mod remote;
pub mod runner;
pub mod sessions;
pub mod transport;

use std::io::IsTerminal as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::cli::{Args, AttachMode, AttachTarget};
use crate::error::TuiError;
use crate::ui::{Effect, Msg, StatusInfo, StatusUpdate, UiChannels};
use crate::workbench::{AppSetup, app};

pub use runner::{ClientConfig, Runner, SpawnPlan};

/// Why the client cannot reach or use the daemon.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Nothing accepts at the endpoint's address.
    #[error("cannot connect to the daemon at {address}: {source}")]
    Connect {
        /// The address.
        address: SocketAddr,
        /// The socket error.
        #[source]
        source: std::io::Error,
    },
    /// A step took too long.
    #[error("timed out {0}")]
    Timeout(&'static str),
    /// The daemon refused the attach (wrong token, or not an attach line).
    #[error("the daemon refused the attach ({code}): {message}")]
    Refused {
        /// The JSON-RPC error code.
        code: i32,
        /// Its message.
        message: String,
    },
    /// The daemon answered something this client does not understand.
    #[error("unexpected answer from the daemon: {0}")]
    Protocol(String),
    /// A call failed.
    #[error("{method} failed ({code}): {message}")]
    Rpc {
        /// The method.
        method: String,
        /// The JSON-RPC error code.
        code: i32,
        /// Its message.
        message: String,
    },
    /// The connection is gone.
    #[error("the connection to the daemon closed")]
    Disconnected,
    /// Reading or writing the connection failed.
    #[error("connection error: {0}")]
    Io(#[source] std::io::Error),
    /// A reply could not be read as the engine's wire type.
    #[error("cannot read the daemon's {what} reply: {source}")]
    Decode {
        /// Which reply.
        what: &'static str,
        /// The serde error.
        #[source]
        source: serde_json::Error,
    },
    /// The process at the daemon's address did not prove it holds the
    /// workspace's token: it may not be the daemon at all (one that took
    /// the port of a daemon that is gone). The token was not sent.
    #[error(
        "the process listening at {address} did not prove it is this workspace's rkat-tui-mob, so the token was not sent: {reason}. If the daemon is gone, another program may hold its port; check what listens there"
    )]
    Unproven {
        /// The address.
        address: SocketAddr,
        /// What it answered.
        reason: String,
    },
    /// The agent has no session.
    #[error("the agent has no session")]
    NoSession,
    /// No daemon answers and `--attach=existing` may not start one.
    #[error(
        "no rkat-tui-mob answers for this workspace (endpoint file {}); start one, or use --attach (auto)",
        file.display()
    )]
    NoDaemon {
        /// The endpoint file.
        file: PathBuf,
    },
    /// The daemon could not be started.
    #[error("cannot start {}: {source}", program.display())]
    Spawn {
        /// The program.
        program: PathBuf,
        /// The error.
        #[source]
        source: std::io::Error,
    },
    /// The started daemon exited before serving.
    #[error("rkat-tui-mob exited ({status}) before serving; see {}", log.display())]
    DaemonExited {
        /// Its exit status.
        status: String,
        /// Its output.
        log: PathBuf,
    },
    /// The started daemon did not write a live endpoint in time.
    #[error("rkat-tui-mob did not come up within {waited:?}; see {}", log.display())]
    NeverCameUp {
        /// How long the client waited.
        waited: Duration,
        /// Its output.
        log: PathBuf,
    },
}

/// [`configure_with`] for `target` on the layers of the workspace it
/// names, read now.
///
/// # Errors
///
/// [`TuiError`] when the workspace, its layers or the realm config cannot
/// be read.
#[cfg(any(test, feature = "test-support"))]
pub async fn configure(args: &Args, target: AttachTarget) -> Result<ClientConfig, TuiError> {
    configure_at(args, target, &crate::client::list::files(None)).await
}

/// [`configure`], with `local_files` as this machine's endpoint files (the
/// ones that establish that a daemon reached by address is local; tests
/// place theirs outside the state directory).
///
/// # Errors
///
/// See [`configure`].
#[cfg(any(test, feature = "test-support"))]
pub async fn configure_at(
    args: &Args,
    target: AttachTarget,
    local_files: &[std::path::PathBuf],
) -> Result<ClientConfig, TuiError> {
    let (args, layers, reached) = retarget(args.clone(), &target, None, local_files).await?;
    let mut config = configure_with(layers, &args, target.mode()).await?;
    if let Some(reached) = reached {
        reached.apply(&mut config);
    }
    Ok(config)
}

/// The arguments and `tui.toml` layers of the workspace `target` attaches
/// to (`layers`: the ones read for `args`, reused when the workspace
/// stays), and the daemon `--attach=HOST:PORT` reached. `check`: refuse a
/// directory without an endpoint file (the start-up does; tests place
/// their endpoint files elsewhere).
async fn retarget(
    args: Args,
    target: &AttachTarget,
    layers: Option<crate::bootstrap::Layers>,
    local_files: &[std::path::PathBuf],
) -> Result<(Args, crate::bootstrap::Layers, Option<address::Reached>), TuiError> {
    let read = |args: &Args, layers: Option<crate::bootstrap::Layers>| match layers {
        Some(layers) => Ok(layers),
        None => crate::bootstrap::read_layers(args),
    };
    match target {
        AttachTarget::Auto | AttachTarget::Existing => {
            let layers = read(&args, layers)?;
            Ok((args, layers, None))
        }
        AttachTarget::Directory(dir) => {
            let args = directory_args(&args, dir)?;
            let layers = crate::bootstrap::read_layers(&args)?;
            Ok((args, layers, None))
        }
        AttachTarget::Address(at) => {
            let reached = address::reach(
                at,
                args.attach_token_file.as_deref(),
                args.attach_insecure,
                local_files,
            )
            .await?;
            // A workspace on this machine: run on it, as a directory
            // attach does; else on the client's own (its history and cost
            // record), with the file features refused.
            let (args, layers) = if reached.remote.is_none() {
                let mut args = args;
                args.context_root = Some(reached.status.workspace.clone());
                let layers = crate::bootstrap::read_layers(&args)?;
                (args, layers)
            } else {
                let layers = read(&args, layers)?;
                (args, layers)
            };
            Ok((args, layers, Some(reached)))
        }
    }
}

/// `args` attached to the workspace of `dir` (`--attach=DIRECTORY`): the
/// workspace the daemon there reports in its endpoint file, else the one
/// `dir` resolves to as the client resolves its own (the nearest
/// ancestor holding a workspace, else `dir`). Everything local then
/// resolves against it: the `!` shell's directory, `@` mentions,
/// `/export`, `/remember`, `/memory` and the input histories. It replaces
/// `-w`.
///
/// # Errors
///
/// [`TuiError::ProjectRoot`] when `dir` cannot be resolved.
pub fn directory_args(args: &Args, dir: &std::path::Path) -> Result<Args, TuiError> {
    let dir = std::fs::canonicalize(dir).map_err(|source| TuiError::ProjectRoot {
        path: dir.to_path_buf(),
        source,
    })?;
    let homes = crate::bootstrap::GlobalDirs::resolve(
        args.config_dir.as_deref(),
        args.user_config_root.as_deref(),
    )
    .map(|global| crate::bootstrap::realm_home::root(&global));
    let resolved = crate::bootstrap::scope::workspace_at(&dir, homes.as_deref());
    let reported = crate::daemon::endpoint::candidates(&resolved)
        .iter()
        .find_map(|file| crate::daemon::endpoint::Endpoint::read(file).ok())
        .and_then(|endpoint| endpoint.workspace)
        .filter(|workspace| workspace.is_dir());
    let mut args = args.clone();
    args.context_root = Some(reported.unwrap_or(resolved));
    Ok(args)
}

/// Resolve what the client needs from `args` and the `tui.toml` `layers`
/// read at start: the workspace, the endpoint and cursor files, the realm
/// config (context windows), how a missing daemon is started and the `!`
/// shell (the user's own, see [`crate::user_shell`]).
///
/// # Errors
///
/// [`TuiError`] when the realm config cannot be read.
pub async fn configure_with(
    layers: crate::bootstrap::Layers,
    args: &Args,
    mode: AttachMode,
) -> Result<ClientConfig, TuiError> {
    let config = crate::bootstrap::load_config(&layers.scope).await?;
    // The `!` shell runs as the user (no sandbox): the client plans no
    // confinement and never re-derives the daemon's.
    let user_shell = std::sync::Arc::new(crate::bootstrap::user_shell(&layers));
    // The layers this process reads for its display and endpoints:
    // the picker lists this process's endpoints, and a switch to a
    // configured one carries its auth binding.
    let crate::bootstrap::Layers {
        scope, mut layered, ..
    } = layers;
    let realm = meerkat_core::connection::RealmId::parse(scope.realm_id.clone()).ok();
    let (mut endpoints, _) =
        crate::endpoints::EndpointRegistry::discover(&config, realm.as_ref(), &layered.endpoints)?;
    // The daemon calls the providers, not this process: a built-in
    // endpoint is offered even when this process holds no key for it.
    endpoints.add_keyless_builtins();
    let spawn = match mode {
        AttachMode::Auto => Some(SpawnPlan {
            program: launch::daemon_program(),
            argv: launch::daemon_argv(args, &scope.context_root),
            log: launch::daemon_log(&scope.realm_root()),
        }),
        AttachMode::Existing => None,
    };
    let pricing = crate::pricing::PricingSetup::load(
        std::mem::take(&mut layered.pricing),
        Some(scope.global.pricing_cache()),
        !args.no_pricing_fetch && layered.pricing_fetch,
    );
    // Drawn only when this client may start the daemon.
    let start_token = match spawn {
        Some(_) => Some(crate::daemon::endpoint::new_token()?),
        None => None,
    };
    Ok(ClientConfig {
        address: None,
        remote: None,
        start: StartAt::from_args(args),
        agent: agent_choice(args),
        start_token,
        endpoint_files: {
            let files = crate::daemon::endpoint::candidates(&scope.context_root);
            if files.is_empty() {
                vec![crate::daemon::endpoint_file(&scope.context_root)?]
            } else {
                files
            }
        },
        cursor: catchup::CursorFile::of_realm(&scope.realm_root()),
        config: Some(config),
        spawn,
        user_shell,
        endpoints,
        lister: crate::models::live_lister(),
        ui: layered.ui,
        compaction: layered.settings.compaction_policy(None),
        pricing,
        scope,
        reroot: None,
    })
}

/// Which conversation the first attach shows: `-c` and no flag the
/// daemon's current one, `--resume` alone the picker, `--resume <ref>` a
/// switch to `ref` (`rkat/resume`, as `/resume <ref>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartAt {
    /// The daemon's current conversation (`-c`, `--resume last`, none).
    Current,
    /// The resume picker over the daemon's conversations (`--resume`).
    Pick,
    /// Switch to this reference once attached (`--resume <ref>`).
    Resume(String),
}

impl StartAt {
    /// What `args` ask for. `-c` and `--resume last` (`~`, `~0`) name the
    /// most recent conversation, which with a daemon is the one it runs:
    /// attaching is all they do.
    #[must_use]
    pub fn from_args(args: &Args) -> Self {
        match &args.resume {
            None => Self::Current,
            Some(None) => Self::Pick,
            Some(Some(reference)) => match reference.trim() {
                "last" | "~" | "~0" => Self::Current,
                _ => Self::Resume(reference.clone()),
            },
        }
    }
}

/// Which agent `args` ask for: with neither `-c` nor `--resume`, a
/// new agent (the daemon's first one when this client started it); `-c`,
/// `--resume last` and `--resume` alone (the picker) the most recently
/// active one; `--resume <ref>` the agent that owns that conversation,
/// seated again on it when that agent quit.
#[must_use]
pub fn agent_choice(args: &Args) -> crate::daemon::protocol::AgentChoice {
    use crate::daemon::protocol::AgentChoice;
    if args.continue_last {
        return AgentChoice::Recent;
    }
    match &args.resume {
        None => AgentChoice::New,
        Some(None) => AgentChoice::Recent,
        Some(Some(reference)) => match reference.trim() {
            "last" | "~" | "~0" => AgentChoice::Recent,
            reference => AgentChoice::Conversation {
                reference: reference.to_string(),
            },
        },
    }
}

/// What the client's UI takes from `config`.
#[must_use]
pub fn app_setup(config: &ClientConfig) -> AppSetup {
    AppSetup {
        compaction: config.compaction.normalized().0,
        ui: config.ui,
        workspace: config.scope.project_root.clone(),
    }
}

/// Run the TUI attached to the workspace's daemon until the user leaves
/// (`/detach`, or `/quit`, which stops the daemon only when this client
/// started it and nobody detached since; a hangup is a detach).
///
/// # Errors
///
/// [`TuiError::NotATerminal`] without a terminal, otherwise when the
/// workspace cannot be resolved or the terminal cannot be driven. A daemon
/// that cannot be reached is reported in the UI.
pub async fn run(
    args: Args,
    target: AttachTarget,
    layers: crate::bootstrap::Layers,
) -> Result<(), TuiError> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(TuiError::NotATerminal);
    }
    if let AttachTarget::Directory(dir) = &target {
        // Before anything is written there: a directory no daemon serves
        // is refused here, not after the UI opened.
        let workspace = directory_args(&args, dir)?.context_root.unwrap_or_default();
        if !crate::daemon::endpoint::candidates(&workspace)
            .iter()
            .any(|file| file.is_file())
        {
            return Err(TuiError::NoDaemonAt { workspace });
        }
    }
    let (args, layers, reached) = retarget(
        args,
        &target,
        Some(layers),
        &crate::client::list::files(None),
    )
    .await?;
    let mut config = configure_with(layers, &args, target.mode()).await?;
    if let Some(reached) = reached {
        reached.apply(&mut config);
    }
    let mode = target.mode();
    config.reroot = Some(runner::Reroot {
        args: args.clone(),
        configure: std::sync::Arc::new(move |args: Args| {
            Box::pin(async move {
                let layers = crate::bootstrap::read_layers(&args)?;
                configure_with(layers, &args, mode).await
            })
        }),
        endpoint_files: rehome::default_endpoint_files(),
    });
    app::init_logging(&config.scope.realm_root())?;
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<Msg>();
    let (effect_tx, effect_rx) = mpsc::unbounded_channel::<Effect>();
    let mut status = StatusInfo::default();
    status.apply(StatusUpdate::Connection(crate::ui::Connection::Connecting));
    let opened = app::open(
        &app_setup(&config),
        status,
        &config.scope,
        args.prompt.clone(),
        &effect_tx,
    );
    // A hangup (the terminal closed) leaves as a detach: the UI restores
    // the terminal and quits without stopping the daemon.
    #[cfg(unix)]
    let hangup = match forward_hangup(ui_tx.clone()) {
        Ok(task) => Some(task),
        Err(err) => {
            tracing::warn!(error = %err, "cannot listen for SIGHUP");
            None
        }
    };
    let runner = Runner::new(config, ui_tx, effect_rx);
    let farewells = runner.farewells();
    // The runner leaves the daemon as `/quit` decided (stopped, or running
    // with the detach recorded), then saves the cursor and the unsent
    // messages.
    let driven = app::drive(
        opened,
        UiChannels { ui_rx },
        effect_tx,
        args.mouse_capture(),
        tokio::spawn(runner.run()),
        Duration::from_secs(5),
    )
    .await;
    #[cfg(unix)]
    if let Some(hangup) = hangup {
        hangup.abort();
    }
    for line in farewells.lock().drain(..) {
        eprintln!("rkat-tui: {line}");
    }
    driven
}

/// Turn every SIGHUP into [`Msg::Hangup`] on `ui_tx` (the UI then leaves as
/// a detach); the handler is installed before this returns.
///
/// # Errors
///
/// When the signal handler cannot be installed.
#[cfg(unix)]
pub fn forward_hangup(
    ui_tx: mpsc::UnboundedSender<Msg>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let mut hangups = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    Ok(tokio::spawn(async move {
        while hangups.recv().await.is_some() {
            if ui_tx.send(Msg::Hangup).is_err() {
                break;
            }
        }
    }))
}

/// Test seam: an attach runner driven headless. Not a stable API.
#[cfg(feature = "test-support")]
pub mod testing {
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;

    use super::{ClientConfig, Runner};
    use crate::ui::{Effect, Msg};

    /// A running attach runner and the UI's ends of its channels.
    pub struct AttachHarness {
        /// Effects, as the UI would send them.
        pub effects: mpsc::UnboundedSender<Effect>,
        /// Everything the runner tells the UI (events included).
        pub ui_rx: mpsc::UnboundedReceiver<Msg>,
        /// The runner; resolves after [`Effect::Quit`], with where the
        /// messages the daemon never got were saved.
        pub task: JoinHandle<Option<crate::workbench::SavedUnsent>>,
    }

    /// Spawn an attach runner on `config`.
    #[must_use]
    pub fn spawn_attach_runner(config: ClientConfig) -> AttachHarness {
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let (effects, effect_rx) = mpsc::unbounded_channel();
        let runner = Runner::new(config, ui_tx, effect_rx);
        AttachHarness {
            effects,
            ui_rx,
            task: tokio::spawn(runner.run()),
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::Parser as _;

    /// `-c` and `--resume last` attach to the daemon's current
    /// conversation; `--resume` alone opens the picker; a reference
    /// switches.
    #[test]
    fn the_start_flags_choose_the_first_conversation() {
        let start = |argv: &[&str]| StartAt::from_args(&Args::try_parse_from(argv).unwrap());
        assert_eq!(start(&["rkat-tui"]), StartAt::Current);
        assert_eq!(start(&["rkat-tui", "-c"]), StartAt::Current);
        assert_eq!(start(&["rkat-tui", "--resume", "last"]), StartAt::Current);
        assert_eq!(start(&["rkat-tui", "--resume", "~"]), StartAt::Current);
        assert_eq!(start(&["rkat-tui", "--resume"]), StartAt::Pick);
        assert_eq!(
            start(&["rkat-tui", "--resume", "01ab"]),
            StartAt::Resume("01ab".into())
        );
        assert_eq!(
            start(&["rkat-tui", "--resume", "~2"]),
            StartAt::Resume("~2".into())
        );
    }

    /// With a live daemon, no flag starts a new agent, `-c` (and `--resume
    /// last`, and the picker) joins the most recent one, `--resume <ref>`
    /// the agent that owns that conversation.
    #[test]
    fn the_start_flags_choose_the_agent() {
        use crate::daemon::protocol::AgentChoice;
        let agent = |argv: &[&str]| agent_choice(&Args::try_parse_from(argv).unwrap());
        assert_eq!(agent(&["rkat-tui"]), AgentChoice::New);
        assert_eq!(agent(&["rkat-tui", "a prompt"]), AgentChoice::New);
        assert_eq!(agent(&["rkat-tui", "-c"]), AgentChoice::Recent);
        assert_eq!(
            agent(&["rkat-tui", "--resume", "last"]),
            AgentChoice::Recent
        );
        assert_eq!(agent(&["rkat-tui", "--resume"]), AgentChoice::Recent);
        assert_eq!(
            agent(&["rkat-tui", "--resume", "01ab"]),
            AgentChoice::Conversation {
                reference: "01ab".into()
            }
        );
    }

    /// A hangup reaches the UI as [`Msg::Hangup`] (which leaves as a
    /// detach, `ui::tests`); the process is not killed by it.
    #[tokio::test]
    async fn a_hangup_reaches_the_ui() {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let task = forward_hangup(ui_tx).unwrap();
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGHUP).unwrap();
        let msg = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("forwarded");
        assert!(matches!(msg, Some(Msg::Hangup)), "{msg:?}");
        task.abort();
    }
}
