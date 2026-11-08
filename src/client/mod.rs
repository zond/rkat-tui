//! Attach mode (experimental, P2 of `docs/mob-split.md`): `rkat-tui
//! --attach` runs the TUI against the workspace's `rkat-tui-mob` daemon
//! instead of hosting the engine in this process.
//!
//! - [`transport`]: the connection (attach handshake with the endpoint
//!   file's token, then a JSON-RPC multiplexer over newline-delimited JSON).
//! - [`launch`]: start-on-demand (probe the endpoint file, start the daemon
//!   in the background when none answers, wait for its endpoint).
//! - [`remote`]: the orchestrator member through the engine's own methods
//!   (`mob/member_send`, `mob/turn_start`, `mob/stream_open`,
//!   `events/list_since`, `mob/stop_member_run`, `mob/force_cancel`).
//! - [`catchup`]: the replay/live de-duplication rule and the per-workspace
//!   cursor.
//! - [`approvals`]: the approval dialogs over the daemon's records
//!   (`approval/list`, `approval/decide`).
//! - [`runner`]: the loop between the UI and the daemon. It serves the same
//!   [`Effect`]/[`Msg`] contract as the in-process runner, so the UI is the
//!   same: approval dialogs, permission modes and `/rules` act on the
//!   daemon's gate (`rkat/*` methods, see [`crate::daemon::local`]); the
//!   user's `!` shell runs here, as in-process; so do the model picker (this
//!   process's endpoints), `/remember` and `/memory` (the files are local;
//!   the new content reaches the orchestrator with
//!   `mob/append_system_context`); `/trust` and `/rules trust` act on the
//!   daemon's trust record (`rkat/trust`). What the daemon cannot do
//!   (`/rewind`, `/compact now`) says why.
//! - [`helpers`]: the follower view of the orchestrator's helpers, from the
//!   mobs' own event streams.
//! - [`sessions`]: `/new`, `/resume` (the daemon's `rkat/new`,
//!   `rkat/resume`, `rkat/conversations`, `rkat/transcript`) and why
//!   `/rewind` is not offered.
//!
//! The in-process runner is not reused under a session trait: it is built
//! on the in-process [`crate::bootstrap::Runtime`] (approval desk, helper
//! host, `fork`, model lists), none of which exists on this side of the
//! wire.

pub mod approvals;
pub mod catchup;
pub mod helpers;
pub mod launch;
pub mod remote;
pub mod runner;
pub mod sessions;
pub mod transport;

use std::io::IsTerminal as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use crate::cli::{Args, AttachMode};
use crate::error::TuiError;
use crate::ui::{App, Effect, History, Msg, StatusInfo, UiChannels, run_ui};

pub use runner::{AttachConfig, AttachRunner, SpawnPlan};

/// Why attach mode cannot reach or use the daemon.
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
        code: i64,
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
        code: i64,
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
    /// The orchestrator has no session.
    #[error("the daemon's orchestrator has no session")]
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
    /// A daemon of an earlier version serves the workspace from the legacy
    /// endpoint file inside it: this client does not start a second one.
    #[error(
        "an rkat-tui-mob of an earlier version (pid {pid}) serves this workspace at {address} from {}, where the agent's shell can read its token; type /daemon stop to stop it (or send SIGTERM to pid {pid}) and this client starts the current daemon and attaches",
        file.display()
    )]
    LegacyDaemon {
        /// Its address.
        address: SocketAddr,
        /// Its process id.
        pid: u32,
        /// The legacy endpoint file.
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

/// Resolve what attach mode needs from `args`: the workspace, the endpoint
/// and cursor files, the realm config (context windows) and how a missing
/// daemon is started.
///
/// # Errors
///
/// [`TuiError`] when the workspace or the realm config cannot be resolved.
pub async fn configure(args: &Args, mode: AttachMode) -> Result<AttachConfig, TuiError> {
    let (scope, config) = crate::bootstrap::prepare(args).await?;
    // The layers the in-process TUI reads for its display and endpoints:
    // the picker lists this process's endpoints, and a switch to a
    // configured one carries its auth binding.
    let (layered, _) = crate::bootstrap::approval::load_layers(&scope, args)?;
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
    let user_shell = crate::bootstrap::user_shell(
        &scope,
        &config,
        args,
        &crate::bootstrap::HostPaths::detect(),
    )
    .map(std::sync::Arc::new)
    .map_err(|err| format!("the ! shell is unavailable: {err}"));
    Ok(AttachConfig {
        endpoint_file: crate::daemon::endpoint_file(&scope.context_root)?,
        cursor: catchup::CursorFile::of_workspace(&scope.context_root),
        config: Some(config),
        spawn,
        user_shell,
        endpoints,
        lister: crate::models::live_lister(),
        ui: layered.ui,
        compaction: layered.settings.compaction_policy(None),
        scope,
    })
}

/// Run the TUI attached to the workspace's daemon until the user quits
/// (which detaches; the daemon keeps running).
///
/// # Errors
///
/// [`TuiError::NotATerminal`] without a terminal, otherwise when the
/// workspace cannot be resolved or the terminal cannot be driven. A daemon
/// that cannot be reached is reported in the UI.
pub async fn run(args: Args, mode: AttachMode) -> Result<(), TuiError> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(TuiError::NotATerminal);
    }
    let config = configure(&args, mode).await?;
    let realm_root = config.scope.realm_root();
    crate::runner::init_logging(&realm_root)?;
    let status = StatusInfo {
        attach: Some(runner::CONNECTING.to_string()),
        ..StatusInfo::default()
    };
    let history = History::load(realm_root.join("tui_history"));
    let global_history = config
        .scope
        .global
        .as_ref()
        .map(|global| History::load(global.history_file()));
    let mut app = App::new(status)
        .with_compaction_policy(config.compaction)
        .with_helper_view(config.ui.helpers)
        .with_checklist_view(config.ui.checklist)
        .with_bell(config.ui.bell)
        .with_workspace(config.scope.project_root.clone())
        .with_history(history.entries().to_vec())
        .with_global_history(
            global_history
                .as_ref()
                .map(|h| h.entries().to_vec())
                .unwrap_or_default(),
        );
    let histories: Vec<History> = std::iter::once(history).chain(global_history).collect();
    // Attach mode sends every event on the integrator channel (in order with
    // the turn messages); the agent channel stays idle.
    let (_agent_tx, agent_rx) = mpsc::channel(1);
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<Msg>();
    let (effect_tx, effect_rx) = mpsc::unbounded_channel::<Effect>();
    if let Some(prompt) = args.prompt.clone() {
        let mut first = app.update(Msg::Paste(prompt));
        first.extend(app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))));
        for effect in first {
            let _ = effect_tx.send(effect);
        }
    }
    let runner = AttachRunner::new(config, ui_tx, effect_rx);
    let runner_task = tokio::spawn(runner.run());
    let ui_result = run_ui(
        app,
        UiChannels { agent_rx, ui_rx },
        effect_tx,
        args.mouse_capture(),
        histories,
        realm_root.join("drafts"),
    )
    .await;
    match tokio::time::timeout(Duration::from_secs(5), runner_task).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => tracing::warn!(error = %err, "attach runner failed"),
        Err(_) => tracing::warn!("attach runner did not shut down in time"),
    }
    ui_result.map_err(TuiError::from)
}

/// Test seam: an attach runner driven headless. Not a stable API.
#[cfg(feature = "test-support")]
pub mod testing {
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;

    use super::{AttachConfig, AttachRunner};
    use crate::ui::{Effect, Msg};

    /// A running attach runner and the UI's ends of its channels.
    pub struct AttachHarness {
        /// Effects, as the UI would send them.
        pub effects: mpsc::UnboundedSender<Effect>,
        /// Everything the runner tells the UI (events included).
        pub ui_rx: mpsc::UnboundedReceiver<Msg>,
        /// The runner; resolves after [`Effect::Quit`].
        pub task: JoinHandle<()>,
    }

    /// Spawn an attach runner on `config`.
    #[must_use]
    pub fn spawn_attach_runner(config: AttachConfig) -> AttachHarness {
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let (effects, effect_rx) = mpsc::unbounded_channel();
        let runner = AttachRunner::new(config, ui_tx, effect_rx);
        AttachHarness {
            effects,
            ui_rx,
            task: tokio::spawn(runner.run()),
        }
    }
}
