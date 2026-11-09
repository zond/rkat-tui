//! The attach-mode loop between the UI and the daemon: [`Effect`]s become
//! the orchestrator's methods ([`super::remote`]), the member's events
//! become [`Msg`]s, and the connection is kept (re-attached with backoff,
//! with catch-up, when it drops).
//!
//! A turn, as the UI sees it, starts with [`Effect::StartTurn`] (or with a
//! run this UI did not start: another client's, or one the catch-up found,
//! announced with [`Msg::RemoteTurn`]) and ends [`RUN_GRACE`] after the last
//! run it saw ended without another starting (a steer that missed the
//! boundary runs as a follow-up run). Its result is the runs' terminal
//! events: `run_completed` (the text and usage), `run_failed` (an
//! interruption when this client interrupted it or the class is
//! `cancelled`, else the failure).
//!
//! Every event is sent to the UI as [`Msg::Agent`] on the integrator
//! channel, so the turn messages stay in order with the events.
//!
//! What both runners do the same way (`!` shell mode, `/remember`,
//! `/memory`, `/export`, prices and the cost record, the model lists and
//! picker, unsent messages) is the [`Workbench`]'s; it reaches the
//! orchestrator through [`Linked`] (`mob/append_system_context`).

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use meerkat_core::lifecycle::RunId;
use meerkat_core::{AgentErrorClass, AgentEvent, EventEnvelope, SessionId};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::ClientError;
use super::approvals::{Approvals, decision_params};
use super::catchup::{Cursor, CursorFile, Mark};
use super::helpers::{self, Action as FollowerAction, Followers, HelperId};
use super::launch::{self, Decision};
use super::remote::{self, ModelSwitch};
use super::sessions;
use super::transport::{ATTACH_TIMEOUT, Connection, Notification};
use crate::approval::desk::{ModeStatus, RuleCommand, RuleOutcome};
use crate::bootstrap::RuntimeScope;
use crate::cli::AttachMode;
use crate::daemon::endpoint::Endpoint;
use crate::daemon::protocol::{self, ModeSet, SetModeParams, TrustOp, TrustParams};
use crate::endpoints::{EndpointRegistry, ModelRefError, WireDialect, parse_model_ref};
use crate::models::{FetchResults, ModelCache};
use crate::ui::delivery::{PendingId, SteerFailure};
use crate::ui::{
    Block, Checklist, ChecklistView, CompactFailure, Effect, EscalationAnswer, EscalationId, Msg,
    NoticeLevel, RuleEffect, StatusUpdate, TurnFailure, TurnOutcome, TurnSummary,
};
use crate::ui::{Departure, QuitDecision};
use crate::user_shell::UserShell;
use crate::workbench::models::Current;
use crate::workbench::{AgentContext, SavedUnsent, Workbench, WorkbenchParts};
use crate::workbench::{context, tasks};

/// The status-bar segment while the first connection is made.
pub const CONNECTING: &str = "connecting";
/// The status-bar segment while a started daemon comes up.
pub const STARTING: &str = "daemon starting";
/// The status-bar segment while a dropped connection is re-made.
pub const RECONNECTING: &str = "reconnecting";

/// Why Up on an empty input takes nothing back in attach mode.
pub const RECALL_UNAVAILABLE: &str = "a message sent to the daemon cannot be taken back in attach mode: the engine's mob/member_send answers no input id to cancel it by (an engine ask, docs/mob-split.md ask 6); the agent gets it at its next step, or interrupt (Esc) to stop the turn";

/// How often the orchestrator's current session is read (another client's
/// `/new` or `/resume` moves it).
pub const SESSION_POLL: Duration = Duration::from_secs(1);

/// How often the mobs are listed while a tool that seats helpers runs.
pub const DISCOVER_EVERY: Duration = Duration::from_millis(250);

/// How long after such a tool starts the mobs are listed.
pub const DISCOVER_FOR: Duration = Duration::from_secs(10);

/// How many events of streams not known yet are held (see
/// [`AttachRunner`]'s `orphans`).
const ORPHANS_MAX: usize = 2_000;

/// The notice shown on the first attach: what the daemon is.
pub const ATTACH_NOTICE: &str = "the agent runs in this workspace's rkat-tui-mob daemon, which keeps working when you /detach and stops on /quit when this rkat-tui started it (its approval requests show in every attached client; the first answer counts).";

/// How long a turn whose runs all ended waits for a follow-up run.
pub const RUN_GRACE: Duration = Duration::from_millis(300);

/// How long after the first interrupt a second one force-cancels.
pub const STOP_GRACE: Duration = Duration::from_secs(1);

/// How long an interrupted turn waits for the daemon to report its end
/// before the UI is given it back.
pub const INTERRUPT_DEADLINE: Duration = Duration::from_secs(10);

/// Bound on the calls that answer at once (sends, stops).
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on a conversation switch (the daemon retires and seats the
/// orchestrator) and on reading the new conversation.
const SWITCH_TIMEOUT: Duration = Duration::from_mins(1);

/// Bound on the calls that decide how to leave (`rkat/status`) and that
/// leave (`rkat/detach`, `rkat/shutdown`).
const LEAVE_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a shutdown that brought no answer is checked for
/// ([`daemon_ended`]).
const STOP_VERIFY: Duration = Duration::from_secs(5);

/// How often a reconnect may start a daemon that died.
const RESTART_EVERY: Duration = Duration::from_secs(30);

/// Reconnect backoff: first and longest wait.
const BACKOFF_FIRST: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(2);

/// The reason recorded with an interrupt.
const INTERRUPT_REASON: &str = "rkat-tui: interrupted by the user";

/// How a missing daemon is started (`--attach=auto`).
#[derive(Debug, Clone)]
pub struct SpawnPlan {
    /// The `rkat-tui-mob` binary.
    pub program: PathBuf,
    /// Its arguments.
    pub argv: Vec<std::ffi::OsString>,
    /// Where its output goes.
    pub log: PathBuf,
}

/// What an attach runner needs.
#[derive(Clone)]
pub struct AttachConfig {
    /// The workspace and realm.
    pub scope: RuntimeScope,
    /// The places of the daemon's endpoint file, in the order the daemon
    /// tries them (the state directory's, then the runtime directory's
    /// fallback); empty only for a daemon reached by address whose
    /// workspace is not on this machine.
    pub endpoint_files: Vec<PathBuf>,
    /// The daemon `--attach=HOST:PORT` reached: always attached here, never
    /// found through the endpoint files nor started.
    pub address: Option<Endpoint>,
    /// Why the daemon's workspace is not on this machine (`--attach=
    /// HOST:PORT` to another host): `!`, `@` mentions, `/remember`,
    /// `/memory` and `/export` to a path are refused with it.
    pub remote: Option<String>,
    /// The workspace's attach cursor.
    pub cursor: CursorFile,
    /// The realm config (context windows); `None`: unknown windows.
    pub config: Option<meerkat_core::Config>,
    /// How to start a missing daemon (`None`: `--attach=existing`).
    pub spawn: Option<SpawnPlan>,
    /// Which conversation the first attach shows (`-c`, `--resume`).
    pub start: super::StartAt,
    /// The start token handed to a daemon this client starts, presented
    /// on every attach: the daemon counts the client that started it as
    /// its owner (`/quit` then stops a daemon nobody detached from).
    pub start_token: Option<String>,
    /// The user's `!` shell (this process's own, as in-process), or why
    /// there is none.
    pub user_shell: Result<Arc<UserShell>, String>,
    /// This process's LLM endpoints (the model picker, `/model`).
    pub endpoints: crate::endpoints::EndpointRegistry,
    /// Fetches the endpoints' model lists (tests list nothing).
    pub lister: crate::models::ModelLister,
    /// The `[ui]` settings of the `tui.toml` layers.
    pub ui: crate::config::UiSettings,
    /// The context-compaction policy of the `tui.toml` layers.
    pub compaction: crate::ui::CompactionPolicy,
    /// The cost readout's prices (the `[pricing]` overrides and the cached
    /// community list; refreshed in the background as in-process).
    pub pricing: crate::pricing::PricingSetup,
}

impl std::fmt::Debug for AttachConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachConfig")
            .field("workspace", &self.scope.context_root)
            .field("endpoint_files", &self.endpoint_files)
            .field("spawn", &self.spawn)
            .field("endpoints", &self.endpoints.names())
            .finish_non_exhaustive()
    }
}

impl AttachConfig {
    /// The attach mode the config stands for.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn mode(&self) -> AttachMode {
        if self.spawn.is_some() {
            AttachMode::Auto
        } else {
            AttachMode::Existing
        }
    }
}

/// One attached connection with the orchestrator's stream open.
struct Link {
    connection: Arc<Connection>,
    notifications: mpsc::UnboundedReceiver<Notification>,
    stream: String,
    session: SessionId,
    endpoint: Endpoint,
}

/// A link and what the catch-up read on it.
struct Established {
    link: Link,
    /// Where the stream resumes (or the render from the start ended).
    mark: Mark,
    /// The conversation rendered from its start, when the stream could not
    /// resume from the saved cursor.
    reset: Option<Vec<Block>>,
    identity: Option<(String, String)>,
    /// The session's total usage `(input, output)`, read after the replay.
    usage: Option<(u64, u64)>,
    /// The live notifications that arrived before the usage answer: their
    /// turns are in `usage` already, so they are shown before the counter
    /// is set to it.
    queued: Vec<Notification>,
    /// The context of the session's last model call when the replay holds
    /// no turn end (the gauge's seed), read from the events before the mark.
    context: Option<u64>,
}

/// A background task's channel back to the runner: every report carries
/// the link epoch it was started on ([`AttachRunner::reporter`]).
struct Reporter {
    tx: mpsc::UnboundedSender<(u64, Report)>,
    epoch: u64,
}

impl Reporter {
    fn send(&self, report: Report) {
        let _ = self.tx.send((self.epoch, report));
    }
}

/// What a background task reports.
enum Report {
    /// A `/model` switch (`rkat/set_model`) applied, or why not.
    ModelSet {
        switch: ModelSwitch,
        result: Result<protocol::ModelSet, String>,
    },
    /// A stop or a cancel failed.
    Interrupt(String),
    /// The daemon's permission mode.
    Mode(ModeStatus),
    /// The mobs the daemon hosts (a discovery round).
    Mobs(Vec<String>),
    /// A mob's stream opened, with its members' sessions; or not.
    MobOpened {
        mob: String,
        opened: Result<(String, Vec<(String, Option<SessionId>)>), String>,
    },
    /// A helper session's durable events.
    Backfilled {
        helper: HelperId,
        events: Vec<EventEnvelope<AgentEvent>>,
    },
    /// A helper's model.
    HelperModel { helper: HelperId, model: String },
    /// The orchestrator's current session, as just read.
    SessionNow(SessionId),
    /// It could not be read.
    SessionUnread,
    /// A conversation switch (this client's, `fresh` for `/new`; `None`:
    /// another client's) and what the new conversation shows.
    Switched {
        result: Result<Adopted, String>,
        own: Option<bool>,
        warning: Option<String>,
    },
    /// The conversation read again from its start ([`AttachRunner::rerender`]).
    Rerendered(Result<Adopted, String>),
    /// The resume picker's rows.
    Picker(Result<Vec<(String, String)>, String>),
    /// The tasks of `session` (`rkat/tasks`), or why they were not read.
    Tasks {
        session: SessionId,
        tasks: Result<Vec<meerkat_tools::builtin::types::Task>, String>,
    },
    /// A conversation switch of this client's did not start (the
    /// conversations could not be listed, the reference resolved to none
    /// or to the current one): why. Ends the switch.
    SwitchRefused(NoticeLevel, String),
}

/// What a client shows of the conversation it switched to.
struct Adopted {
    session: SessionId,
    /// The orchestrator's stream, opened again.
    stream: String,
    blocks: Vec<Block>,
    /// The last durable sequence of the session (shown as the transcript).
    last_seq: u64,
    identity: Option<(String, String)>,
    /// The session's total usage `(input, output)`.
    usage: Option<(u64, u64)>,
    /// The context of its last model call (the gauge).
    context: Option<u64>,
}

/// The UI's current turn.
#[derive(Debug, Default)]
struct Turn {
    active: bool,
    /// Runs started and not ended.
    open_runs: HashSet<RunId>,
    /// The run to stop.
    current_run: Option<RunId>,
    /// A run of this turn started.
    saw_run: bool,
    /// Steers admitted and not yet delivered, oldest first.
    steers: VecDeque<PendingId>,
    result: Option<Result<TurnSummary, TurnFailure>>,
    interrupted_at: Option<Instant>,
    force_cancelled: bool,
    /// Every run ended: the turn ends then unless another starts.
    finish_by: Option<Instant>,
    /// An interrupt the daemon has not confirmed ends the turn then.
    give_up_at: Option<Instant>,
}

impl Turn {
    fn record(&mut self, end: Result<TurnSummary, TurnFailure>) {
        self.result = Some(crate::ui::merge_turn_end(self.result.take(), end));
    }

    fn deadline(&self) -> Option<Instant> {
        match (self.finish_by, self.give_up_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// Why a call to the daemon brought no answer.
#[derive(Debug, thiserror::Error)]
enum CallFailure {
    /// No connection now.
    #[error("not attached to the daemon")]
    NotAttached,
    /// No answer within the call's bound.
    #[error("the daemon did not answer in time")]
    TimedOut,
    /// The call failed.
    #[error(transparent)]
    Failed(ClientError),
}

/// `call`, bounded by `limit`.
async fn bounded<T>(
    limit: Duration,
    call: impl std::future::Future<Output = Result<T, ClientError>>,
) -> Result<T, CallFailure> {
    tokio::time::timeout(limit, call)
        .await
        .map_err(|_| CallFailure::TimedOut)?
        .map_err(CallFailure::Failed)
}

/// Events of streams opened but not registered yet, by stream id, at most
/// [`ORPHANS_MAX`] of them; past that they are dropped, and that is logged
/// once per connection.
#[derive(Debug)]
struct Orphans<T> {
    by_stream: std::collections::HashMap<String, Vec<T>>,
    held: usize,
    warned: bool,
}

impl<T> Default for Orphans<T> {
    fn default() -> Self {
        Self {
            by_stream: std::collections::HashMap::new(),
            held: 0,
            warned: false,
        }
    }
}

impl<T> Orphans<T> {
    /// Hold `event` of `stream` until the stream is registered.
    fn hold(&mut self, stream: String, event: T) {
        if self.held >= ORPHANS_MAX {
            if !std::mem::replace(&mut self.warned, true) {
                tracing::warn!(
                    held = self.held,
                    "too many events of streams not registered yet; dropping more"
                );
            }
            return;
        }
        self.held += 1;
        self.by_stream.entry(stream).or_default().push(event);
    }

    /// The events held for `stream`, oldest first.
    fn take(&mut self, stream: &str) -> Vec<T> {
        let events = self.by_stream.remove(stream).unwrap_or_default();
        self.held -= events.len();
        events
    }

    /// Forget everything (the connection's streams are gone).
    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// The attach-mode runner.
pub struct AttachRunner {
    config: AttachConfig,
    ui_tx: mpsc::UnboundedSender<Msg>,
    effect_rx: mpsc::UnboundedReceiver<Effect>,
    link: Option<Link>,
    connecting: Option<JoinHandle<Result<Established, ClientError>>>,
    /// The daemon may still be started (only the first connect does).
    first_connect: bool,
    reports_tx: mpsc::UnboundedSender<(u64, Report)>,
    reports_rx: mpsc::UnboundedReceiver<(u64, Report)>,
    /// Counts the links made: a report started on an earlier link (a
    /// connection lost since) names a stream or a session of that
    /// connection and is not applied ([`AttachRunner::on_report`]).
    link_epoch: u64,
    mark: Option<Mark>,
    turn: Turn,
    /// `(provider, model)` as last read or switched to.
    model: Option<(String, String)>,
    /// `/daemon stop` was sent: a lost connection is not re-made.
    stopping: bool,
    /// The daemon the events shown came from (the cursor names it, see
    /// [`super::catchup::daemon_id`]: a restarted daemon numbers its events
    /// in a new space).
    daemon_id_seen: Option<String>,
    /// The approval dialogs shown.
    approvals: Approvals,
    /// The orchestrator's helpers.
    followers: Followers,
    /// Events of streams opened but not registered yet, by stream id.
    orphans: Orphans<meerkat_mob::AttributedEvent>,
    /// When the mobs are listed next, and until when the listing repeats.
    discover_at: Option<Instant>,
    discover_until: Option<Instant>,
    /// A listing is running.
    discovering: bool,
    /// When the orchestrator's current session is read next.
    session_check_at: Option<Instant>,
    checking_session: bool,
    /// A conversation switch (this client's, or adopting another's) runs.
    switching: bool,
    /// The last `/model` switch (carried to a `/new` conversation).
    last_switch: Option<ModelSwitch>,
    /// The endpoint the status bar names.
    current_endpoint: Option<String>,
    /// `!` shell mode, `/remember`, `/memory`, `/export`, prices and the
    /// cost record, the model lists and unsent messages.
    bench: Workbench,
    /// The steers the daemon admitted: the agent has them, or gets them
    /// after this client leaves, so quitting saves none of them.
    admitted: Vec<PendingId>,
    /// What to say on the terminal once the UI is gone (how this client
    /// left the daemon).
    farewells: Farewells,
    /// Prompts sent before the first attach finished (the command line's
    /// prompt is sent as the UI opens): held until the first link is up
    /// and no start-up switch runs, or the first connect failed; `None`
    /// once released.
    held: Option<Vec<Effect>>,
    /// The first connect failed (held prompts then fail as not attached).
    first_failed: bool,
}

/// Lines for the terminal after the UI closed ([`AttachRunner::farewells`]).
pub type Farewells = Arc<parking_lot::Mutex<Vec<String>>>;

impl AttachRunner {
    /// A runner on `config`; it connects once [`AttachRunner::run`] runs.
    #[must_use]
    pub fn new(
        config: AttachConfig,
        ui_tx: mpsc::UnboundedSender<Msg>,
        effect_rx: mpsc::UnboundedReceiver<Effect>,
    ) -> Self {
        let (reports_tx, reports_rx) = mpsc::unbounded_channel();
        let bench = Workbench::new(WorkbenchParts {
            ui_tx: ui_tx.clone(),
            scope: config.scope.clone(),
            user_shell: config.user_shell.clone(),
            pricing: config.pricing.clone(),
            endpoints: config.endpoints.clone(),
            lister: Arc::clone(&config.lister),
            models: None,
        });
        Self {
            config,
            ui_tx,
            effect_rx,
            link: None,
            connecting: None,
            first_connect: true,
            reports_tx,
            reports_rx,
            link_epoch: 0,
            mark: None,
            turn: Turn::default(),
            model: None,
            stopping: false,
            daemon_id_seen: None,
            approvals: Approvals::default(),
            followers: Followers::default(),
            orphans: Orphans::default(),
            discover_at: None,
            discover_until: None,
            discovering: false,
            session_check_at: None,
            checking_session: false,
            switching: false,
            last_switch: None,
            current_endpoint: None,
            bench,
            admitted: Vec::new(),
            farewells: Farewells::default(),
            held: Some(Vec::new()),
            first_failed: false,
        }
    }

    /// What the runner leaves to say on the terminal once the UI is gone
    /// (whether the daemon keeps running).
    #[must_use]
    pub fn farewells(&self) -> Farewells {
        Arc::clone(&self.farewells)
    }

    /// Say `text` now and again on the terminal after the UI closed.
    fn farewell(&self, level: NoticeLevel, text: String) {
        self.notice(level, text.clone());
        self.farewells.lock().push(text);
    }

    /// Serve effects until the UI quits, then detach (the daemon and any
    /// running turn go on). Returns where the messages the daemon never
    /// got were saved, when there were any.
    pub async fn run(mut self) -> Option<SavedUnsent> {
        self.bench.start();
        self.status(CONNECTING);
        self.connect();
        loop {
            for effect in self.releasable() {
                self.handle_effect(effect).await;
            }
            tokio::select! {
                effect = self.effect_rx.recv() => match effect {
                    None | Some(Effect::Quit) => break,
                    Some(effect @ Effect::StartTurn(_)) if self.held.is_some() => {
                        if let Some(held) = &mut self.held {
                            held.push(effect);
                        }
                    }
                    Some(effect) => self.handle_effect(effect).await,
                },
                note = next_notification(&mut self.link) => match note {
                    Some(note) => self.on_notification(&note),
                    None => self.lost(),
                },
                joined = join(&mut self.connecting) => {
                    self.connecting = None;
                    self.connected(joined);
                }
                Some((epoch, report)) = self.reports_rx.recv() => self.on_report(epoch, report),
                () = sleep_until(self.turn.deadline()) => self.on_deadline(),
                () = sleep_until(self.discover_at) => self.discover(),
                () = sleep_until(self.session_check_at) => self.check_session(),
                joined = self.bench.background() => self.model_lists(joined),
            }
        }
        self.save_cursor();
        self.bench.cancel_shell();
        if let Some(task) = self.connecting.take() {
            task.abort();
        }
        self.bench.save_unsent(&self.admitted)
    }

    fn send(&self, msg: Msg) {
        let _ = self.ui_tx.send(msg);
    }

    /// The held prompts, once they may go: the first link is up and no
    /// start-up switch runs, or the first connect failed.
    fn releasable(&mut self) -> Vec<Effect> {
        let ready = self.first_failed || (self.link.is_some() && !self.switching);
        if ready {
            self.held.take().unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// The channel a background task started now reports on.
    fn reporter(&self) -> Reporter {
        Reporter {
            tx: self.reports_tx.clone(),
            epoch: self.link_epoch,
        }
    }

    fn notice(&self, level: NoticeLevel, text: impl Into<String>) {
        self.send(Msg::Notice {
            level,
            text: text.into(),
        });
    }

    fn status(&self, state: &str) {
        self.send(Msg::Status(StatusUpdate::Attach(Some(state.to_string()))));
    }

    /// Start connecting in the background: the first time with
    /// start-on-demand, afterwards as a reconnect with backoff.
    fn connect(&mut self) {
        let files = self.config.endpoint_files.clone();
        let saved = self
            .mark
            .as_ref()
            .map(|mark| mark.cursor(self.daemon_id_seen.as_deref()))
            .or_else(|| self.config.cursor.load());
        let ui_tx = self.ui_tx.clone();
        let start_token = self.config.start_token.clone();
        let task = if let Some(endpoint) = self.config.address.clone() {
            // Reached by address: the same address again, with backoff.
            let first = std::mem::take(&mut self.first_connect);
            tokio::spawn(async move {
                let mut wait = BACKOFF_FIRST;
                loop {
                    match establish(endpoint.clone(), saved.as_ref(), start_token.as_deref()).await
                    {
                        Ok(established) => return Ok(established),
                        Err(err) if first => return Err(err),
                        Err(err) => tracing::debug!(error = %err, "reconnect failed"),
                    }
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(BACKOFF_MAX);
                }
            })
        } else if std::mem::take(&mut self.first_connect) {
            let spawn = self.config.spawn.clone();
            tokio::spawn(async move {
                let endpoint =
                    first_endpoint(&files, spawn.as_ref(), start_token.as_deref(), &ui_tx).await?;
                establish(endpoint, saved.as_ref(), start_token.as_deref()).await
            })
        } else {
            let spawn = self.config.spawn.clone();
            tokio::spawn(async move {
                let mut wait = BACKOFF_FIRST;
                let mut restarted: Option<Instant> = None;
                loop {
                    match launch::probe_any(&files).await {
                        launch::Liveness::Live(endpoint) => {
                            match establish(endpoint, saved.as_ref(), start_token.as_deref()).await
                            {
                                Ok(established) => return Ok(established),
                                Err(err) => tracing::debug!(error = %err, "reconnect failed"),
                            }
                        }
                        // The daemon died and left its endpoint file:
                        // start it again (auto mode), at most every
                        // RESTART_EVERY. A daemon that shut down removed
                        // its file and is waited for.
                        launch::Liveness::Stale(_)
                            if restarted.is_none_or(|at| at.elapsed() >= RESTART_EVERY) =>
                        {
                            if let Some(plan) = &spawn {
                                restarted = Some(Instant::now());
                                match restart(&files, plan, &ui_tx).await {
                                    Ok(endpoint) => match establish(
                                        endpoint,
                                        saved.as_ref(),
                                        start_token.as_deref(),
                                    )
                                    .await
                                    {
                                        Ok(established) => return Ok(established),
                                        Err(err) => {
                                            tracing::debug!(error = %err, "reattach failed");
                                        }
                                    },
                                    Err(err) => {
                                        let _ = ui_tx.send(Msg::Notice {
                                            level: NoticeLevel::Error,
                                            text: format!(
                                                "the daemon could not be restarted: {err}"
                                            ),
                                        });
                                    }
                                }
                            }
                        }
                        launch::Liveness::Stale(_) | launch::Liveness::Absent => {}
                    }
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(BACKOFF_MAX);
                }
            })
        };
        self.connecting = Some(task);
    }

    fn connected(
        &mut self,
        joined: Result<Result<Established, ClientError>, tokio::task::JoinError>,
    ) {
        let established = match joined {
            Ok(Ok(established)) => established,
            Ok(Err(err)) => {
                self.first_failed = true;
                self.send(Msg::Status(StatusUpdate::Attach(Some("detached".into()))));
                self.notice(
                    NoticeLevel::Error,
                    format!("cannot attach to the workspace daemon: {err}"),
                );
                return;
            }
            Err(err) => {
                self.first_failed = true;
                tracing::warn!(error = %err, "the connect task failed");
                return;
            }
        };
        let Established {
            link,
            mark,
            reset,
            identity,
            usage,
            queued,
            context,
        } = established;
        let address = link.endpoint.address;
        let daemon_id = super::catchup::daemon_id(&link.endpoint.token);
        // A daemon this client has not shown yet (the first attach, or one
        // that restarted): what its start-up had to say.
        let new_daemon = self.daemon_id_seen.as_deref() != Some(daemon_id.as_str());
        self.daemon_id_seen = Some(daemon_id);
        let session = link.session.to_string();
        let fresh = self.mark.is_none();
        // A render from the start replaces what was shown; a resumed
        // stream goes on from the mark the cursor was saved from.
        if reset.is_some()
            || self
                .mark
                .as_ref()
                .is_none_or(|current| current.session_id() != mark.session_id())
        {
            self.mark = Some(mark);
        }
        self.send(Msg::Status(StatusUpdate::Session(session)));
        if fresh {
            self.notice(NoticeLevel::Info, ATTACH_NOTICE);
        } else {
            self.notice(
                NoticeLevel::Info,
                format!("reattached to the daemon at {address}"),
            );
        }
        if let Some((model, provider)) = identity {
            self.followers.set_default_model(&model);
            self.set_model_status(&provider, &model);
        }
        if fresh {
            self.load_cost(&link.session);
        }
        self.status(&format!("attached {address}"));
        // The helpers of this conversation: what was shown before a
        // reconnect stays shown.
        if fresh || self.followers.owner() != Some(&link.session) {
            self.followers.reset(link.session.clone());
        } else {
            self.followers.streams_lost();
        }
        self.orphans.clear();
        self.link = Some(link);
        self.link_epoch += 1;
        if let Some(blocks) = reset {
            self.send(Msg::ResetTranscript(blocks));
        }
        // The gauge, as a switch seeds it: the replay since the cursor may
        // hold no turn end although the session has a context.
        if context.is_some() {
            self.send(Msg::Status(StatusUpdate::Context {
                used: context,
                approx: false,
            }));
        }
        // What the live stream delivered before the usage answer: its turns
        // are in the totals.
        for note in queued {
            self.on_notification(&note);
        }
        // The counters are the session's totals, as in-process; after the
        // replay and the live events queued before the usage read, whose
        // turns they already hold. Live turn ends after this add to them.
        if let Some((input, output)) = usage {
            self.send(Msg::Status(StatusUpdate::Tokens { input, output }));
        }
        let now = Instant::now();
        self.discover_at = Some(now);
        self.session_check_at = Some(now + SESSION_POLL);
        self.refresh_mode();
        self.refresh_tasks();
        if new_daemon {
            self.startup_notices();
        }
        if fresh {
            match std::mem::replace(&mut self.config.start, super::StartAt::Current) {
                super::StartAt::Current => {}
                super::StartAt::Pick => self.show_resume_picker(),
                super::StartAt::Resume(reference) => self.resume_session(reference),
            }
        }
    }

    /// Show the daemon's start-up notices (`rkat/notices`), as the
    /// in-process TUI shows its own at start.
    fn startup_notices(&self) {
        let Some(connection) = self.connection() else {
            return;
        };
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let read = bounded(
                CALL_TIMEOUT,
                remote::local::<_, protocol::Notices>(
                    &connection,
                    protocol::NOTICES,
                    &serde_json::json!({}),
                ),
            )
            .await;
            match read {
                Ok(read) => {
                    for notice in read.notices {
                        let level = if notice.warn {
                            NoticeLevel::Warn
                        } else {
                            NoticeLevel::Info
                        };
                        let _ = ui_tx.send(Msg::Notice {
                            level,
                            text: notice.text,
                        });
                    }
                }
                Err(err) => {
                    tracing::debug!(error = %err, "cannot read the daemon's start-up notices");
                }
            }
        });
    }

    /// Read the daemon's permission mode into the status bar.
    fn refresh_mode(&self) {
        let Some(connection) = self.connection() else {
            return;
        };
        let reports = self.reporter();
        tokio::spawn(async move {
            let read = bounded(
                CALL_TIMEOUT,
                remote::local(&connection, protocol::GET_MODE, &serde_json::json!({})),
            )
            .await;
            if let Ok(status) = read {
                reports.send(Report::Mode(status));
            }
        });
    }

    fn push_mode(&self, status: ModeStatus) {
        self.send(Msg::Status(StatusUpdate::Mode {
            mode: status.mode,
            paused: status.paused,
            denials: status.denials,
        }));
    }

    /// The connection is gone: re-make it (unless the daemon was told to
    /// stop). A running turn goes on in the daemon; the catch-up shows it.
    fn lost(&mut self) {
        self.link = None;
        self.save_cursor();
        self.followers.streams_lost();
        self.orphans.clear();
        self.discover_at = None;
        self.session_check_at = None;
        self.switching = false;
        // What is still pending shows again after the reattach.
        for id in self.approvals.clear() {
            self.send(Msg::EscalationResolved(id));
        }
        if self.stopping {
            self.send(Msg::Status(StatusUpdate::Attach(Some(
                "daemon stopped".into(),
            ))));
            self.notice(
                NoticeLevel::Info,
                "the daemon has shut down; /quit to leave",
            );
            if self.turn.active {
                self.turn.record(Err(TurnFailure::Interrupted));
                self.finish_turn();
            }
            return;
        }
        self.status(RECONNECTING);
        self.connect();
    }

    fn on_notification(&mut self, note: &Notification) {
        let Some(link) = &self.link else {
            return;
        };
        if note.method == protocol::DECISION_NOTICE {
            let session = link.session.clone();
            if let Some(change) = self.approvals.notice(&session, &note.params) {
                for msg in change.messages() {
                    self.send(msg);
                }
            }
            return;
        }
        if note.method == protocol::FILE_DIFF {
            match serde_json::from_value::<protocol::FileDiffNote>(note.params.clone()) {
                Ok(diff) => self.send(Msg::Diff {
                    tool_use_id: diff.tool_use_id,
                    path: diff.path,
                    unified: diff.unified,
                    added: diff.added,
                    removed: diff.removed,
                }),
                Err(err) => tracing::debug!(error = %err, "unreadable file diff"),
            }
            return;
        }
        if note.method == protocol::DECISION_ENDED {
            if let Some(change) = self.approvals.decision_ended(&note.params) {
                for msg in change.messages() {
                    self.send(msg);
                }
            }
            return;
        }
        if let Some((envelope, epoch)) = remote::stream_event(note, &link.stream) {
            self.on_event(envelope, epoch.as_deref());
            return;
        }
        if let Some(stream) = remote::stream_end(note) {
            self.followers.ended(stream);
            return;
        }
        let Some((stream, attributed)) = remote::attributed_event(note) else {
            return;
        };
        if helpers::starts_helpers(&attributed.envelope.payload) {
            self.start_discovery();
        }
        if self.followers.owns(&stream) {
            let actions = self.followers.event(&stream, attributed);
            self.follow(actions);
        } else {
            // A stream whose open has not answered yet.
            self.orphans.hold(stream, attributed);
        }
    }

    /// Carry out what the followers ask.
    fn follow(&self, actions: Vec<FollowerAction>) {
        for action in actions {
            match action {
                FollowerAction::Ui(msg) => self.send(msg),
                FollowerAction::Backfill {
                    helper,
                    session,
                    after,
                } => {
                    let Some(connection) = self.connection() else {
                        continue;
                    };
                    let reports = self.reporter();
                    tokio::spawn(async move {
                        let events =
                            bounded(CALL_TIMEOUT, replay_to_end(&connection, &session, after))
                                .await
                                .ok()
                                .map(|(events, _)| events)
                                .unwrap_or_default();
                        reports.send(Report::Backfilled { helper, events });
                    });
                }
                FollowerAction::Snapshot { helper, session } => {
                    let Some(connection) = self.connection() else {
                        continue;
                    };
                    let reports = self.reporter();
                    tokio::spawn(async move {
                        if let Ok((model, _)) =
                            bounded(CALL_TIMEOUT, remote::identity(&connection, &session)).await
                        {
                            reports.send(Report::HelperModel { helper, model });
                        }
                    });
                }
            }
        }
    }

    /// A tool that seats helpers started: list the mobs for a while, so a
    /// new one (an implicit `delegate` mob) is followed.
    fn start_discovery(&mut self) {
        let now = Instant::now();
        self.discover_until = Some(now + DISCOVER_FOR);
        if self.discover_at.is_none() {
            self.discover_at = Some(now);
        }
    }

    /// List the daemon's mobs and follow the new ones.
    fn discover(&mut self) {
        self.discover_at = None;
        let Some(connection) = self.connection() else {
            return;
        };
        if self.discovering {
            return;
        }
        self.discovering = true;
        let reports = self.reporter();
        tokio::spawn(async move {
            let mobs = bounded(CALL_TIMEOUT, remote::mobs(&connection))
                .await
                .unwrap_or_default();
            reports.send(Report::Mobs(mobs));
        });
    }

    /// Open `mob`'s mob-wide stream and read its members' sessions.
    fn open_mob(&self, mob: String) {
        let Some(connection) = self.connection() else {
            return;
        };
        let reports = self.reporter();
        tokio::spawn(async move {
            let opened = bounded(CALL_TIMEOUT, async {
                let stream = remote::open_mob_stream(&connection, &mob).await?;
                let mut members = Vec::new();
                for label in remote::members(&connection, &mob).await.unwrap_or_default() {
                    let session = remote::member_session(&connection, &mob, &label).await.ok();
                    members.push((label, session));
                }
                Ok::<_, ClientError>((stream, members))
            })
            .await
            .map_err(|err| err.to_string());
            reports.send(Report::MobOpened { mob, opened });
        });
    }

    /// Read the orchestrator's current session (another client may have
    /// switched it).
    fn check_session(&mut self) {
        self.session_check_at = None;
        let Some(connection) = self.connection() else {
            return;
        };
        if self.checking_session || self.switching {
            self.session_check_at = Some(Instant::now() + SESSION_POLL);
            return;
        }
        self.checking_session = true;
        let reports = self.reporter();
        tokio::spawn(async move {
            reports.send(
                match bounded(CALL_TIMEOUT, remote::session_id(&connection)).await {
                    Ok(session) => Report::SessionNow(session),
                    Err(_) => Report::SessionUnread,
                },
            );
        });
    }

    fn set_model_status(&mut self, provider: &str, model: &str) {
        self.show_model(provider, WireDialect::parse(provider), model);
    }

    /// The status bar names `model` on `endpoint` (of `dialect`).
    fn show_model(&mut self, endpoint: &str, dialect: Option<WireDialect>, model: &str) {
        self.model = Some((endpoint.to_string(), model.to_string()));
        self.current_endpoint = Some(endpoint.to_string());
        self.send(Msg::Status(StatusUpdate::Model {
            provider: endpoint.to_string(),
            model: model.to_string(),
        }));
        let window = self.config.config.as_ref().and_then(|config| {
            let provider = dialect?.into();
            crate::endpoints::registry_context_window(config, provider, model)
        });
        self.send(Msg::Status(StatusUpdate::ContextWindow(window)));
    }

    fn save_cursor(&self) {
        if let Some(mark) = &self.mark
            && mark.sequence() > 0
        {
            self.config
                .cursor
                .save(&mark.cursor(self.daemon_id_seen.as_deref()));
        }
    }

    /// One of the member's events (live, or replayed after the cursor; of
    /// the space `epoch`): shown once ([`Mark`]), and what it means for the
    /// turn. A stream that reports it lost events the member no longer
    /// retains (the cursor was older than its window) is rendered again
    /// from the conversation's start.
    fn on_event(&mut self, envelope: EventEnvelope<AgentEvent>, epoch: Option<&str>) {
        if matches!(envelope.payload, AgentEvent::StreamTruncated { .. }) {
            if let Some(link) = &self.link
                && !self.switching
            {
                let session = link.session.clone();
                self.rerender(session);
            }
            return;
        }
        if let Some(mark) = &mut self.mark
            && !mark.admit(epoch, envelope.seq)
        {
            return;
        }
        if helpers::starts_helpers(&envelope.payload) {
            self.start_discovery();
        }
        let mut terminal = false;
        // A run already going when this client attached (its start was shown
        // before it detached): the UI is busy until its end.
        if !self.turn.active && joins_a_run(&envelope.payload) {
            self.turn = Turn {
                active: true,
                saw_run: true,
                ..Turn::default()
            };
            self.send(Msg::RemoteTurn { prompt: None });
        }
        match &envelope.payload {
            AgentEvent::RunStarted {
                identity, input, ..
            } => {
                if !self.turn.active {
                    self.turn = Turn {
                        active: true,
                        ..Turn::default()
                    };
                    self.send(Msg::RemoteTurn {
                        prompt: input.prompt_text(),
                    });
                } else if self.turn.saw_run {
                    // A follow-up run: it carries the steers that missed
                    // the boundary.
                    for id in std::mem::take(&mut self.turn.steers) {
                        self.send(Msg::SteerDelivered(id));
                    }
                }
                self.turn.saw_run = true;
                self.turn.finish_by = None;
                if let Some(run) = &identity.run_id {
                    self.turn.open_runs.insert(run.clone());
                    self.turn.current_run = Some(run.clone());
                }
            }
            AgentEvent::ToolExecutionCompleted { name, .. }
                if helpers::HELPER_TOOLS.contains(&name.as_str()) =>
            {
                // Once more: a mob made while the tool ran.
                self.discover_at = Some(Instant::now());
            }
            AgentEvent::ToolExecutionCompleted { name, .. } if tasks::changes_tasks(name) => {
                self.refresh_tasks();
            }
            AgentEvent::BoundaryAppendApplied { .. } => {
                // First in, first out: `mob/member_send` answered no input
                // id to match this event's with, so with two clients
                // steering one turn this can mark the other's steer (an
                // ask, docs/mob-split.md ask 6).
                if let Some(id) = self.turn.steers.pop_front() {
                    self.send(Msg::SteerDelivered(id));
                }
            }
            AgentEvent::RunCompleted {
                identity,
                result,
                usage,
                ..
            } => {
                if let Some(run) = &identity.run_id {
                    self.turn.open_runs.remove(run);
                }
                let usage = usage.as_usage();
                self.turn.record(Ok(TurnSummary {
                    text: result.clone(),
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                }));
                terminal = true;
            }
            AgentEvent::RunFailed {
                identity,
                error_report,
                ..
            } => {
                if let Some(run) = &identity.run_id {
                    self.turn.open_runs.remove(run);
                }
                let interrupted = self.turn.interrupted_at.is_some()
                    || error_report.class == AgentErrorClass::Cancelled;
                self.turn.record(Err(if interrupted {
                    TurnFailure::Interrupted
                } else {
                    TurnFailure::Failed(error_report.message.clone())
                }));
                terminal = true;
            }
            _ => {}
        }
        self.send(Msg::Agent(envelope));
        if terminal {
            self.save_cursor();
            if self.turn.active && self.turn.open_runs.is_empty() {
                self.turn.finish_by = Some(Instant::now() + RUN_GRACE);
            }
        }
    }

    fn on_deadline(&mut self) {
        let now = Instant::now();
        if self.turn.finish_by.is_some_and(|at| at <= now) {
            self.finish_turn();
        } else if self.turn.give_up_at.is_some_and(|at| at <= now) {
            self.notice(
                NoticeLevel::Warn,
                "the daemon did not confirm the interrupt; the run may still be going (it shows here when it reports)",
            );
            self.turn.record(Err(TurnFailure::Interrupted));
            self.finish_turn();
        }
    }

    fn finish_turn(&mut self) {
        let turn = std::mem::take(&mut self.turn);
        if !turn.active {
            return;
        }
        // Steers still held went to the runtime; it owns them now.
        for id in turn.steers {
            self.send(Msg::SteerDelivered(id));
        }
        self.send(Msg::TurnFinished(TurnOutcome::plain(
            turn.result.unwrap_or_else(|| Ok(TurnSummary::default())),
        )));
        self.save_cursor();
        // The denial counters may have moved.
        self.refresh_mode();
    }

    /// A background task's `report`, started on the link `epoch`. What a
    /// task started on an earlier link learned about that connection (a
    /// switched-to stream, a mob stream, the session it read) is dropped:
    /// installed, a dead connection's stream id would silently mute the
    /// orchestrator; the reattach read everything again.
    fn on_report(&mut self, epoch: u64, report: Report) {
        if epoch != self.link_epoch {
            match report {
                Report::Switched { .. } | Report::Rerendered(_) | Report::MobOpened { .. } => {
                    tracing::debug!("dropped a report of a connection lost since");
                    return;
                }
                // The loss ended that switch; one started since is not.
                Report::SwitchRefused(level, text) => {
                    self.notice(level, text);
                    return;
                }
                Report::SessionNow(_) | Report::SessionUnread => {
                    self.checking_session = false;
                    return;
                }
                _ => {}
            }
        }
        match report {
            Report::ModelSet { switch, result } => match result {
                Ok(set) => {
                    if self
                        .link
                        .as_ref()
                        .is_some_and(|link| link.session == set.session_id)
                    {
                        let dialect = WireDialect::parse(&set.provider);
                        self.show_model(&switch.endpoint, dialect, &set.model);
                    }
                    self.notice(
                        NoticeLevel::Info,
                        format!("switched to {} on {}", set.model, switch.endpoint),
                    );
                    self.last_switch = Some(switch);
                }
                Err(why) => self.notice(NoticeLevel::Error, format!("model not changed: {why}")),
            },
            Report::Interrupt(err) => {
                self.notice(NoticeLevel::Error, format!("interrupt incomplete: {err}"));
            }
            Report::Mode(status) => self.push_mode(status),
            Report::Mobs(mobs) => {
                self.discovering = false;
                for mob in mobs {
                    if self.followers.should_open(&mob) {
                        self.open_mob(mob);
                    }
                }
                let now = Instant::now();
                if self.discover_until.is_some_and(|until| until > now)
                    && self.discover_at.is_none()
                {
                    self.discover_at = Some(now + DISCOVER_EVERY);
                }
            }
            Report::MobOpened { mob, opened } => match opened {
                Ok((stream, members)) => {
                    if self.link.is_none() {
                        return;
                    }
                    self.followers.opened(stream.clone(), &mob);
                    let mut actions = Vec::new();
                    for event in self.orphans.take(&stream) {
                        actions.extend(self.followers.event(&stream, event));
                    }
                    for (label, session) in members {
                        actions.extend(self.followers.member(&mob, &label, session));
                    }
                    self.follow(actions);
                }
                Err(why) => {
                    tracing::debug!(%mob, %why, "cannot follow the mob");
                    self.followers.open_failed(&mob);
                }
            },
            Report::Backfilled { helper, events } => {
                let actions = self.followers.backfilled(&helper, events);
                self.follow(actions);
            }
            Report::HelperModel { helper, model } => {
                let actions = self.followers.model(&helper, model);
                self.follow(actions);
            }
            Report::SessionNow(session) => {
                self.checking_session = false;
                if self.link.is_some() {
                    self.session_check_at = Some(Instant::now() + SESSION_POLL);
                }
                let moved = self
                    .link
                    .as_ref()
                    .is_some_and(|link| link.session != session);
                if moved && !self.switching {
                    self.adopt_switch(session, None, None);
                }
            }
            Report::SessionUnread => {
                self.checking_session = false;
                if self.link.is_some() {
                    self.session_check_at = Some(Instant::now() + SESSION_POLL);
                }
            }
            Report::Switched {
                result,
                own,
                warning,
            } => {
                self.switching = false;
                match result {
                    Ok(adopted) => self.adopted(adopted, own, warning, false),
                    Err(why) if own.is_some() => self.notice(
                        NoticeLevel::Error,
                        format!("could not switch conversations: {why}"),
                    ),
                    Err(why) => {
                        tracing::warn!(%why, "cannot show the orchestrator's new conversation");
                    }
                }
            }
            Report::Rerendered(result) => {
                self.switching = false;
                match result {
                    Ok(adopted) => self.adopted(adopted, None, None, true),
                    Err(why) => self.notice(
                        NoticeLevel::Warn,
                        format!("the stream lost events and the conversation could not be read again: {why}"),
                    ),
                }
            }
            Report::Picker(Ok(items)) => self.send(Msg::ShowResumePicker(items)),
            Report::Picker(Err(why)) => self.notice(
                NoticeLevel::Error,
                format!("could not list the conversations: {why}"),
            ),
            Report::SwitchRefused(level, text) => {
                self.switching = false;
                self.notice(level, text);
            }
            Report::Tasks { session, tasks } => {
                // A list read for a conversation left since is not shown.
                if self
                    .link
                    .as_ref()
                    .is_none_or(|link| link.session != session)
                {
                    return;
                }
                self.send(match tasks {
                    Ok(tasks) => Msg::Checklist(Checklist::from_tasks(&tasks)),
                    Err(why) => {
                        tracing::warn!(%why, "cannot read the session's tasks");
                        Msg::ChecklistUnavailable(why)
                    }
                });
            }
        }
    }

    /// `/context`: the UI's `header` plus what the context holds, as
    /// in-process ([`crate::workbench::context`]): the conversation's
    /// messages from the daemon (`rkat/transcript`), the instruction files
    /// the daemon's session builds read (this workspace's, found here).
    fn context_report(&self, header: String) {
        let link = self
            .link
            .as_ref()
            .map(|link| (Arc::clone(&link.connection), link.session.clone()));
        let scope = self.config.scope.clone();
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let messages = match link {
                Some((connection, session)) => {
                    bounded(CALL_TIMEOUT, remote::transcript(&connection, &session))
                        .await
                        .map_err(|err| err.to_string())
                }
                None => Err("not attached to the daemon".to_string()),
            };
            let sources = crate::bootstrap::InstructionSources::of(&scope);
            let files = context::instruction_files(&scope, &sources);
            let text = context::context_breakdown(
                &header,
                messages.as_deref().map_err(Clone::clone),
                &files,
                "rkat-tui's file and note tools, in the daemon",
            );
            let _ = ui_tx.send(Msg::Notice {
                level: NoticeLevel::Info,
                text,
            });
        });
    }

    /// Read the conversation's tasks from the daemon (`rkat/tasks`) for the
    /// Ctrl+T checklist, as the in-process runner reads its session's.
    fn refresh_tasks(&self) {
        let Some(link) = &self.link else {
            return;
        };
        let connection = Arc::clone(&link.connection);
        let session = link.session.clone();
        let reports = self.reporter();
        tokio::spawn(async move {
            let params = protocol::SessionParams {
                session_id: session.clone(),
            };
            let tasks = bounded(
                CALL_TIMEOUT,
                remote::local::<_, protocol::Tasks>(&connection, protocol::TASKS, &params),
            )
            .await
            .map(|read| read.tasks)
            .map_err(|err| err.to_string());
            reports.send(Report::Tasks { session, tasks });
        });
    }

    /// Ctrl+T: the daemon records the view as the realm's `[ui] checklist`
    /// (the realm file and its trust record are the daemon's).
    fn set_checklist_view(&self, view: ChecklistView) {
        const UNSAVED: &str = "the checklist view applies to this run but was not saved";
        let Some(connection) = self.connection() else {
            self.notice(
                NoticeLevel::Warn,
                format!("{UNSAVED}: not attached to the daemon"),
            );
            return;
        };
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let params = protocol::ChecklistViewParams { view };
            let saved = bounded(
                CALL_TIMEOUT,
                remote::local::<_, serde_json::Value>(
                    &connection,
                    protocol::CHECKLIST_VIEW,
                    &params,
                ),
            )
            .await;
            if let Err(err) = saved {
                let _ = ui_tx.send(Msg::Notice {
                    level: NoticeLevel::Warn,
                    text: format!("{UNSAVED}: {err}"),
                });
            }
        });
    }

    /// Whether `what` is refused because the daemon's workspace is not on
    /// this machine (said with the reason when it is).
    fn local_refused(&self, what: &str) -> bool {
        let Some(why) = &self.config.remote else {
            return false;
        };
        self.notice(
            NoticeLevel::Warn,
            format!("{what} edits the daemon's workspace, which is not on this machine: {why}"),
        );
        true
    }

    /// `text` with its `@` mentions expanded from the daemon's workspace;
    /// as typed (and said why) when that workspace is not on this machine.
    fn expand_mentions(&self, text: &str) -> String {
        let Some(why) = &self.config.remote else {
            return self.bench.expand_mentions(text);
        };
        let mentions = text
            .split_whitespace()
            .any(|word| word.len() > 1 && word.starts_with('@'));
        if mentions {
            self.notice(
                NoticeLevel::Warn,
                format!(
                    "@ mentions were sent as typed, not expanded: the daemon's workspace is not on this machine ({why})"
                ),
            );
        }
        text.to_string()
    }

    fn connection(&self) -> Option<Arc<Connection>> {
        self.link.as_ref().map(|link| Arc::clone(&link.connection))
    }

    async fn handle_effect(&mut self, effect: Effect) {
        match effect {
            Effect::StartTurn(text) => self.start_turn(&text).await,
            Effect::Steer { id, text } => self.steer(id, &text).await,
            Effect::Interrupt => self.interrupt(),
            Effect::RecallQueued => self.send(Msg::Recalled {
                recalled: Vec::new(),
                errors: vec![RECALL_UNAVAILABLE.to_string()],
            }),
            Effect::RunShell { id, command } => match &self.config.remote {
                Some(why) => self.send(Msg::ShellFinished {
                    id,
                    outcome: crate::ui::ShellOutcome::Failed(format!(
                        "! commands run in the daemon's workspace, which is not on this machine: {why}"
                    )),
                }),
                None => self.bench.run_shell(id, command, self.linked()),
            },
            Effect::CancelShell => self.bench.cancel_shell(),
            Effect::SetModel(reference) => self.set_model(&reference),
            Effect::CompactNow => self.compact_now(),
            Effect::StatusReport => self.status_report(),
            Effect::ContextReport(header) => self.context_report(header),
            Effect::Export { path: Some(_), .. } if self.local_refused("/export to a path") => {}
            Effect::Remember(_) if self.local_refused("/remember") => {}
            Effect::OpenMemory(_) if self.local_refused("/memory") => {}
            Effect::Export { path, text } => {
                let session = self
                    .link
                    .as_ref()
                    .map(|link| link.session.to_string())
                    .unwrap_or_default();
                self.bench.export(path.as_deref(), &text, &session);
            }
            Effect::DaemonStop => self.daemon_stop().await,
            Effect::DecideQuit => self.decide_quit().await,
            Effect::Leave(departure) => self.depart(departure).await,
            Effect::NewSession => self.switch_conversation(None),
            Effect::ResumeSession(reference) => self.resume_session(reference),
            Effect::RequestResumePicker => self.show_resume_picker(),
            Effect::RequestRewindMenu | Effect::Rewind(_) => {
                self.notice(NoticeLevel::Warn, sessions::REWIND_UNAVAILABLE);
            }
            Effect::RequestModelPicker { refresh } => {
                let current = current(self.current_endpoint.as_deref(), self.model.as_ref());
                self.bench.request_model_picker(refresh, current);
            }
            Effect::Remember(text) => self.bench.remember(&text, &self.linked()).await,
            Effect::OpenMemory(target) => self.bench.open_memory(target),
            Effect::MemoryEdited { target, path } => {
                let linked = self.linked();
                self.bench.memory_edited(target, &path, &linked).await;
            }
            Effect::CostSnapshot(snapshot) => {
                if let Some(link) = &self.link {
                    self.bench.save_cost(&link.session.to_string(), &snapshot);
                }
            }
            Effect::RespondEscalation(id, answer) => self.respond_escalation(id, &answer),
            Effect::SetMode(mode) => self.set_mode(mode.name()).await,
            Effect::ResumeAutoMode => {
                self.set_mode(crate::approval::mode::PermissionMode::Auto.name())
                    .await;
            }
            Effect::AddRule {
                effect,
                tool,
                pattern,
            } => {
                let command = RuleCommand::Add {
                    deny: effect == RuleEffect::Deny,
                    tool,
                    pattern,
                };
                self.outcome_command(protocol::RULES, &command, "/rules")
                    .await;
            }
            Effect::RemoveRule(n) => {
                self.outcome_command(protocol::RULES, &RuleCommand::Remove { n }, "/rules")
                    .await;
            }
            Effect::ListRules => {
                self.outcome_command(protocol::RULES, &RuleCommand::List, "/rules")
                    .await;
            }
            Effect::TrustRealmRules => {
                let params = TrustParams { op: TrustOp::Rules };
                self.outcome_command(protocol::TRUST, &params, "/rules trust")
                    .await;
            }
            Effect::TrustProject => {
                let params = TrustParams {
                    op: TrustOp::Project,
                };
                self.outcome_command(protocol::TRUST, &params, "/trust")
                    .await;
            }
            Effect::SaveUnsent(held) => self.bench.hold_unsent(held),
            Effect::SetChecklistView(view) => self.set_checklist_view(view),
            Effect::Quit => {}
        }
    }

    /// The UI answered an approval dialog: decide its decision.
    fn respond_escalation(&mut self, id: EscalationId, answer: &EscalationAnswer) {
        if let Some((decision, digest)) = self.approvals.answer_decision(id) {
            self.decide(id, &decision, &digest, answer);
        }
    }

    /// The UI answered the dialog `id` of `decision`: decide it. A decision
    /// another client (or the daemon, when the engine gave up on it)
    /// settled first is refused by the engine (`not_open`) and the answer
    /// is dropped; "deny and stop" also interrupts the turn.
    fn decide(
        &mut self,
        id: EscalationId,
        decision: &str,
        digest: &str,
        answer: &EscalationAnswer,
    ) {
        self.send(Msg::EscalationResolved(id));
        let Some(connection) = self.connection() else {
            self.notice(
                NoticeLevel::Warn,
                "not attached: the answer was not sent (the request shows again after the reattach while it is open)",
            );
            return;
        };
        let params = decision_params(decision, digest, answer);
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let decided = bounded(
                CALL_TIMEOUT,
                remote::local::<_, serde_json::Value>(
                    &connection,
                    protocol::DECISION_DECIDE,
                    &params,
                ),
            )
            .await;
            let text = match decided {
                Ok(_) => return,
                Err(CallFailure::Failed(ClientError::Rpc { message, .. })) => {
                    tracing::debug!(%message, "decision already settled");
                    (
                        NoticeLevel::Info,
                        format!(
                            "that approval request was already settled; your answer was not used ({message})"
                        ),
                    )
                }
                Err(err) => (NoticeLevel::Warn, format!("the answer was not sent: {err}")),
            };
            let _ = ui_tx.send(Msg::Notice {
                level: text.0,
                text: text.1,
            });
        });
        if matches!(answer, EscalationAnswer::Stop) {
            self.interrupt();
        }
    }

    /// `/compact now`: ask the orchestrator's next compaction check to
    /// compact (`mob/compact_member`, no rebuild); its next model request
    /// compacts, and the gauge follows the `compaction_completed` event.
    /// A member the engine refuses (dormant after a daemon restart, before
    /// its first run) is asked again through its session
    /// (`session/compact`), which refuses the same way when it is not live.
    fn compact_now(&self) {
        let Some(link) = &self.link else {
            self.send(Msg::CompactRequested(Err(CompactFailure::Request(
                "not attached to the daemon".to_string(),
            ))));
            return;
        };
        let connection = Arc::clone(&link.connection);
        let session = link.session.clone();
        let ui_tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let asked = tokio::time::timeout(CALL_TIMEOUT, async {
                match remote::compact_member(&connection).await {
                    Ok(()) => Ok(()),
                    Err(member) => remote::compact_session(&connection, &session)
                        .await
                        .map_err(|session| format!("{member}; {session}")),
                }
            })
            .await
            .unwrap_or_else(|_| Err("the daemon did not answer in time".to_string()));
            let _ = ui_tx.send(Msg::CompactRequested(
                asked.map_err(CompactFailure::Request),
            ));
        });
    }

    /// Shift+Tab, `/mode`, `/auto resume`: switch the daemon's mode.
    async fn set_mode(&mut self, mode: &str) {
        let params = SetModeParams {
            mode: mode.to_string(),
        };
        match self
            .call_local::<_, ModeSet>(protocol::SET_MODE, &params)
            .await
        {
            Ok(set) => {
                self.push_mode(set.status);
                if let Some(warning) = set.warning {
                    self.notice(NoticeLevel::Warn, warning);
                }
                self.notice(NoticeLevel::Info, set.notice);
            }
            Err(CallFailure::NotAttached) => {
                self.notice(NoticeLevel::Warn, "not attached: the mode was not changed");
            }
            Err(err) => self.notice(NoticeLevel::Error, format!("mode not changed: {err}")),
        }
    }

    /// A command whose answer is a [`RuleOutcome`] (`/rules …` on the
    /// daemon's engine, `/trust` and `/rules trust` on its trust record),
    /// named `name` in what goes wrong.
    async fn outcome_command<P: serde::Serialize>(
        &self,
        method: &'static str,
        params: &P,
        name: &str,
    ) {
        match self.call_local::<_, RuleOutcome>(method, params).await {
            Ok(outcome) => {
                let level = if outcome.ok {
                    NoticeLevel::Info
                } else {
                    NoticeLevel::Error
                };
                self.notice(level, outcome.text);
            }
            Err(CallFailure::NotAttached) => self.notice(
                NoticeLevel::Warn,
                format!("not attached: {name} needs the daemon"),
            ),
            Err(err) => self.notice(NoticeLevel::Error, format!("{name} failed: {err}")),
        }
    }

    /// Call the daemon's own `method` (one of [`protocol`]'s) with
    /// `params`, bounded by [`CALL_TIMEOUT`].
    async fn call_local<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &'static str,
        params: &P,
    ) -> Result<R, CallFailure> {
        let connection = self.connection().ok_or(CallFailure::NotAttached)?;
        bounded(CALL_TIMEOUT, remote::local(&connection, method, params)).await
    }

    /// The orchestrator as the workbench reaches it, over this connection.
    fn linked(&self) -> Linked {
        Linked(self.connection())
    }

    async fn start_turn(&mut self, text: &str) {
        let Some(connection) = self.connection() else {
            self.send(Msg::TurnFinished(TurnOutcome::plain(Err(
                TurnFailure::Failed(
                    "not attached to the daemon (see the status bar); try again once it is".into(),
                ),
            ))));
            return;
        };
        self.turn = Turn {
            active: true,
            ..Turn::default()
        };
        let text = self.expand_mentions(text);
        let Err(failure) = bounded(CALL_TIMEOUT, remote::send(&connection, &text, false)).await
        else {
            return;
        };
        self.turn.record(Err(TurnFailure::Failed(format!(
            "the prompt was not sent: {failure}"
        ))));
        self.finish_turn();
    }

    async fn steer(&mut self, id: PendingId, text: &str) {
        if !self.turn.active {
            self.send(Msg::SteerAck {
                id,
                result: Err(SteerFailure::NotRunning),
            });
            return;
        }
        let Some(connection) = self.connection() else {
            self.send(Msg::SteerAck {
                id,
                result: Err(SteerFailure::Rejected("not attached to the daemon".into())),
            });
            return;
        };
        let text = self.expand_mentions(text);
        let result = match bounded(CALL_TIMEOUT, remote::send(&connection, &text, true)).await {
            Ok(()) => {
                self.turn.steers.push_back(id);
                self.admitted.push(id);
                Ok(())
            }
            Err(err) => Err(SteerFailure::Rejected(err.to_string())),
        };
        self.send(Msg::SteerAck { id, result });
    }

    /// Esc: stop the run the stream reported; a second Esc after
    /// [`STOP_GRACE`] (or a first one before any run was reported)
    /// force-cancels the member.
    fn interrupt(&mut self) {
        if !self.turn.active {
            return;
        }
        let now = Instant::now();
        let Some(connection) = self.connection() else {
            self.turn.record(Err(TurnFailure::Interrupted));
            self.finish_turn();
            return;
        };
        let reports = self.reporter();
        let stop = match (&self.turn.interrupted_at, &self.turn.current_run) {
            (None, Some(run)) if self.turn.open_runs.contains(run) => Some(run.to_string()),
            (None, _) => None,
            (Some(first), _)
                if now.duration_since(*first) >= STOP_GRACE && !self.turn.force_cancelled =>
            {
                None
            }
            (Some(_), _) => return,
        };
        if self.turn.interrupted_at.is_none() {
            self.turn.interrupted_at = Some(now);
            self.turn.give_up_at = Some(now + INTERRUPT_DEADLINE);
        }
        if stop.is_none() {
            self.turn.force_cancelled = true;
        }
        tokio::spawn(async move {
            let result = match stop {
                Some(run) => bounded(
                    CALL_TIMEOUT,
                    remote::stop_run(&connection, &run, INTERRUPT_REASON),
                )
                .await
                .map(drop),
                None => bounded(CALL_TIMEOUT, remote::force_cancel(&connection)).await,
            };
            if let Err(failure) = result {
                reports.send(Report::Interrupt(failure.to_string()));
            }
        });
    }

    /// `/model <reference>` or a picker selection: the daemon switches the
    /// session in place ([`AttachRunner::switch_model`], with the model,
    /// provider and the endpoint's auth binding); it keeps the model.
    /// A bare id is resolved against this process's endpoints as
    /// in-process (their lists fetched first when several could serve it).
    fn set_model(&mut self, reference: &str) {
        match model_switch(
            reference,
            &self.config.endpoints,
            self.bench.models(),
            self.current_endpoint.as_deref(),
        ) {
            Ok(switch) => self.switch_model(switch),
            Err(err) => {
                if !self.bench.defer_until_listed(reference) {
                    self.notice(NoticeLevel::Warn, err.to_string());
                }
            }
        }
    }

    /// Switch the orchestrator's session to `switch` in place
    /// (`rkat/set_model`), as the in-process `/model` does: refused while a
    /// turn runs; the next prompt is an ordinary send.
    fn switch_model(&self, switch: ModelSwitch) {
        if self.turn.active {
            self.notice(
                NoticeLevel::Warn,
                "a turn is running; switch models once it ends (or interrupt it)",
            );
            return;
        }
        let Some(connection) = self.connection() else {
            self.notice(
                NoticeLevel::Warn,
                "not attached to the daemon: the model was not changed",
            );
            return;
        };
        let reports = self.reporter();
        tokio::spawn(async move {
            let params = protocol::SetModelParams {
                model: switch.model.clone(),
                provider: switch.provider.clone(),
                auth_binding: switch.auth_binding.clone(),
            };
            let result = bounded(
                SWITCH_TIMEOUT,
                remote::local::<_, protocol::ModelSet>(&connection, protocol::SET_MODEL, &params),
            )
            .await
            .map_err(|err| match err {
                CallFailure::Failed(ClientError::Rpc { message, .. }) => message,
                other => other.to_string(),
            });
            reports.send(Report::ModelSet { switch, result });
        });
    }

    /// The model lists arrived ([`Workbench::background`]): a `/model
    /// <ref>` that waited for them switches now.
    fn model_lists(&mut self, joined: Result<FetchResults, tokio::task::JoinError>) {
        let current = current(self.current_endpoint.as_deref(), self.model.as_ref());
        if let Some(reference) = self.bench.lists_arrived(joined, current) {
            self.set_model(&reference);
        }
    }

    /// `/new` (`target: None`) or `/resume <session>`: ask the daemon to
    /// switch the orchestrator's conversation, then show the new one.
    fn switch_conversation(&mut self, target: Option<SessionId>) {
        if !self.may_switch() {
            return;
        }
        let Some(link) = &self.link else {
            return;
        };
        self.switching = true;
        let connection = Arc::clone(&link.connection);
        let stream = link.stream.clone();
        let reports = self.reporter();
        let fresh = target.is_none();
        tokio::spawn(async move {
            reports.send(switch_and_adopt(&connection, &stream, target.as_ref(), fresh).await);
        });
    }

    /// Whether a conversation switch may start now (else says why).
    fn may_switch(&self) -> bool {
        if self.turn.active {
            self.notice(
                NoticeLevel::Warn,
                "a turn is running; interrupt it first (Esc)",
            );
            return false;
        }
        if self.switching {
            self.notice(NoticeLevel::Info, "a conversation switch is under way");
            return false;
        }
        if self.link.is_none() {
            self.notice(
                NoticeLevel::Warn,
                "not attached to the daemon (see the status bar)",
            );
            return false;
        }
        true
    }

    /// Show the conversation the orchestrator moved to (another client
    /// switched it).
    fn adopt_switch(&mut self, session: SessionId, own: Option<bool>, warning: Option<String>) {
        let Some(link) = &self.link else {
            return;
        };
        self.switching = true;
        let connection = Arc::clone(&link.connection);
        let stream = link.stream.clone();
        let reports = self.reporter();
        tokio::spawn(async move {
            let result = adopt(&connection, &stream, session).await;
            reports.send(Report::Switched {
                result,
                own,
                warning,
            });
        });
    }

    /// Show the conversation `session` from its start again (the stream
    /// lost events): the stream reopened live, the committed transcript.
    fn rerender(&mut self, session: SessionId) {
        let Some(link) = &self.link else {
            return;
        };
        self.switching = true;
        let connection = Arc::clone(&link.connection);
        let stream = link.stream.clone();
        let reports = self.reporter();
        tokio::spawn(async move {
            let result = adopt(&connection, &stream, session).await;
            reports.send(Report::Rerendered(result));
        });
    }

    /// The orchestrator runs as `adopted.session` now: follow it, and show
    /// its transcript, model and cost.
    fn adopted(
        &mut self,
        adopted: Adopted,
        own: Option<bool>,
        warning: Option<String>,
        rerender: bool,
    ) {
        let Adopted {
            session,
            stream,
            blocks,
            last_seq,
            identity,
            usage,
            context,
        } = adopted;
        let Some(link) = &mut self.link else {
            return;
        };
        link.session = session.clone();
        link.stream = stream;
        // The old conversation's turn and dialogs are over.
        if self.turn.active {
            self.turn.record(Err(TurnFailure::Interrupted));
            self.finish_turn();
        }
        for id in self.approvals.clear() {
            self.send(Msg::EscalationResolved(id));
        }
        let id = session.to_string();
        self.mark = Some(Mark::rendered(&id, last_seq));
        self.save_cursor();
        self.followers.reset(session.clone());
        self.orphans.clear();
        self.send(Msg::Status(StatusUpdate::Session(id.clone())));
        self.send(Msg::ResetTranscript(blocks));
        // The conversation's own totals and gauge, as the in-process
        // switch reads them from the session.
        let (input, output) = usage.unwrap_or_default();
        self.send(Msg::Status(StatusUpdate::Tokens { input, output }));
        self.send(Msg::Status(StatusUpdate::Context {
            used: context,
            approx: false,
        }));
        self.load_cost(&session);
        self.refresh_tasks();
        if let Some((model, provider)) = identity {
            self.followers.set_default_model(&model);
            self.set_model_status(&provider, &model);
            // A fresh conversation starts on the daemon's start-up model:
            // this client's last `/model` carries over, as in-process.
            if own == Some(true)
                && let Some(switch) = self.last_switch.clone()
                && switch.model != model
            {
                self.switch_model(switch);
            }
        }
        let short = id.get(..8).unwrap_or(&id).to_string();
        let text = match own {
            _ if rerender => format!(
                "the stream lost events the daemon no longer keeps; conversation {short} is shown from its start"
            ),
            Some(true) => {
                format!("a new conversation ({short}); the previous one stays in /resume")
            }
            Some(false) => format!("resumed conversation {short}"),
            None => format!("another client switched the agent to conversation {short}"),
        };
        self.notice(NoticeLevel::Info, text);
        if let Some(warning) = warning {
            self.notice(NoticeLevel::Warn, warning);
        }
        self.discover_at = Some(Instant::now());
    }

    /// `/resume <reference>`: resolve it against the conversations, then
    /// switch.
    fn resume_session(&mut self, reference: String) {
        if !self.may_switch() {
            return;
        }
        let Some(link) = &self.link else {
            return;
        };
        self.switching = true;
        let connection = Arc::clone(&link.connection);
        let stream = link.stream.clone();
        let current = link.session.clone();
        let reports = self.reporter();
        tokio::spawn(async move {
            let listed = match remote::conversations(&connection).await {
                Ok(listed) => listed,
                Err(err) => {
                    reports.send(Report::SwitchRefused(
                        NoticeLevel::Error,
                        format!("could not list the conversations: {err}"),
                    ));
                    return;
                }
            };
            let target = match sessions::resolve(&reference, &listed) {
                Ok(target) if target == current => {
                    reports.send(Report::SwitchRefused(
                        NoticeLevel::Info,
                        "already in this conversation".to_string(),
                    ));
                    return;
                }
                Ok(target) => target,
                Err(why) => {
                    reports.send(Report::SwitchRefused(NoticeLevel::Warn, why));
                    return;
                }
            };
            reports.send(switch_and_adopt(&connection, &stream, Some(&target), false).await);
        });
    }

    /// `/resume` alone: the picker over the conversations.
    fn show_resume_picker(&self) {
        let Some(connection) = self.connection() else {
            self.notice(NoticeLevel::Warn, "not attached to the daemon");
            return;
        };
        let reports = self.reporter();
        tokio::spawn(async move {
            let listed = bounded(CALL_TIMEOUT, remote::conversations(&connection))
                .await
                .map_err(|err| err.to_string());
            reports.send(Report::Picker(
                listed.map(|listed| sessions::picker_items(&listed)),
            ));
        });
    }

    /// Show `session`'s stored cost totals (none: zero).
    fn load_cost(&self, session: &SessionId) {
        let cost = self
            .bench
            .recorded_cost(&session.to_string())
            .unwrap_or_default();
        self.send(Msg::Status(StatusUpdate::CostTotals(cost)));
    }

    fn status_report(&self) {
        let mut lines = vec![format!(
            "rkat-tui {} (attached to the workspace daemon)",
            env!("CARGO_PKG_VERSION")
        )];
        match &self.link {
            Some(link) => {
                lines.push(format!(
                    "daemon: {} (pid {}), mob `{}`, orchestrator `{}`",
                    link.endpoint.address,
                    link.endpoint.pid,
                    crate::daemon::MOB_ID,
                    crate::daemon::ORCHESTRATOR
                ));
                lines.push(format!("session: {}", link.session));
            }
            None => lines.push("daemon: not attached".to_string()),
        }
        if let Some((provider, model)) = &self.model {
            lines.push(format!("model: {provider}/{model}"));
        }
        match (&self.config.remote, &self.config.address) {
            (Some(why), _) => lines.push(format!(
                "workspace: the daemon's, not on this machine ({why}); local files: {}",
                self.config.scope.context_root.display()
            )),
            (None, address) => lines.push(format!(
                "workspace: {}{}",
                self.config.scope.context_root.display(),
                if address.is_some() {
                    " (daemon reached by address)"
                } else {
                    ""
                }
            )),
        }
        let files: Vec<String> = self
            .config
            .endpoint_files
            .iter()
            .map(|file| file.display().to_string())
            .collect();
        lines.push(format!("endpoint file: {}", files.join(", then ")));
        lines.push(
            "approval gate: in the daemon (requests show here; /rules and Shift+Tab act on the daemon)"
                .to_string(),
        );
        self.notice(NoticeLevel::Info, lines.join("\n"));
    }

    /// `/daemon stop`: SIGTERM to the pid the endpoint file names, once the
    /// file still names the daemon this client is attached to.
    async fn daemon_stop(&mut self) {
        if self.config.address.is_some() {
            // Its pid may be another machine's: ask it.
            let Some(connection) = self.connection() else {
                self.notice(NoticeLevel::Warn, "not attached to the daemon");
                return;
            };
            let asked = bounded(
                LEAVE_TIMEOUT,
                remote::local::<_, serde_json::Value>(
                    &connection,
                    protocol::SHUTDOWN,
                    &serde_json::json!({}),
                ),
            )
            .await;
            match asked {
                Ok(_) | Err(CallFailure::Failed(ClientError::Disconnected)) => {
                    self.stopping = true;
                    self.notice(NoticeLevel::Info, "asked the daemon to shut down");
                }
                Err(err) => self.notice(
                    NoticeLevel::Warn,
                    format!("the daemon was not stopped: {err}"),
                ),
            }
            return;
        }
        match self.sigterm_attached().await {
            Ok(pid) => self.notice(
                NoticeLevel::Info,
                format!("asked the daemon (pid {pid}) to shut down"),
            ),
            Err(why) => self.notice(NoticeLevel::Warn, why),
        }
    }

    /// SIGTERM to the daemon this client is attached to (the endpoint
    /// file's pid, when its address is the attached one).
    async fn sigterm_attached(&mut self) -> Result<u32, String> {
        if self.config.address.is_some() {
            return Err(
                "the daemon was reached by address: its pid may be another machine's, so it is not signalled; /daemon stop asks it to shut down".to_string(),
            );
        }
        let connected = self.link.as_ref().map(|link| link.endpoint.address);
        let liveness = launch::probe_any(&self.config.endpoint_files).await;
        let pid = stop_target(&liveness, connected)?;
        terminate(pid).map_err(|err| format!("cannot stop the daemon: {err}"))?;
        self.stopping = true;
        Ok(pid)
    }

    /// `/quit`: how to leave, from the daemon's `rkat/status`
    /// ([`quit_decision`]); answered with [`Msg::QuitDecided`].
    async fn decide_quit(&mut self) {
        // The daemon is gone already (`/daemon stop`): just leave.
        let status = match (self.stopping, self.connection()) {
            (false, Some(connection)) => bounded(
                LEAVE_TIMEOUT,
                remote::local::<_, protocol::Status>(
                    &connection,
                    protocol::STATUS,
                    &serde_json::json!({}),
                ),
            )
            .await
            .map_err(|err| tracing::info!(error = %err, "the daemon's status is unknown at quit"))
            .ok(),
            _ => None,
        };
        let decision = if self.stopping {
            QuitDecision::Leave
        } else {
            quit_decision(status.as_ref())
        };
        if let (QuitDecision::Leave, Some(status)) = (&decision, &status) {
            let others = status.clients.saturating_sub(1);
            self.farewell(
                NoticeLevel::Info,
                format!(
                    "{others} other client{} attached: detached; the daemon (pid {}) keeps running for {}",
                    if others == 1 { " is" } else { "s are" },
                    status.pid,
                    if others == 1 { "it" } else { "them" },
                ),
            );
        }
        self.send(Msg::QuitDecided(decision));
    }

    /// Leave the daemon as the UI decided: record a detach, or stop it.
    async fn depart(&mut self, departure: Departure) {
        let Some(connection) = self.connection() else {
            if departure == Departure::Terminate && !self.stopping {
                self.farewell(
                    NoticeLevel::Warn,
                    "not attached: the daemon was not stopped (/daemon stop from an attached rkat-tui, or SIGTERM)".to_string(),
                );
            }
            return;
        };
        let endpoint = self.link.as_ref().map(|link| link.endpoint.clone());
        let pid = endpoint.as_ref().map_or(0, |endpoint| endpoint.pid);
        let method = match departure {
            Departure::Detach => protocol::DETACH,
            Departure::Terminate => protocol::SHUTDOWN,
        };
        let answered = bounded(
            LEAVE_TIMEOUT,
            remote::local::<_, serde_json::Value>(&connection, method, &serde_json::json!({})),
        )
        .await;
        match (departure, answered) {
            (Departure::Detach, Ok(_)) => self.farewell(
                NoticeLevel::Info,
                format!("detached: the agent keeps running in the daemon (pid {pid}); rkat-tui --attach reattaches"),
            ),
            (Departure::Detach, Err(err)) => self.farewell(
                NoticeLevel::Warn,
                format!("detached; the daemon (pid {pid}) was not told ({err}) and keeps running"),
            ),
            (Departure::Terminate, Ok(_)) => {
                self.stopping = true;
                self.farewell(NoticeLevel::Info, format!("stopped the daemon (pid {pid})"));
            }
            // No answer (the connection dropped, before or after the
            // daemon read the request, or the call timed out): whether it
            // stops is unknown until its endpoint file says so.
            (
                Departure::Terminate,
                Err(
                    err @ (CallFailure::NotAttached
                    | CallFailure::TimedOut
                    | CallFailure::Failed(ClientError::Disconnected)),
                ),
            ) => {
                tracing::info!(error = %err, "rkat/shutdown brought no answer; checking whether the daemon ended");
                let ended = match &endpoint {
                    Some(endpoint) => {
                        daemon_ended(
                            &self.config.endpoint_files,
                            endpoint,
                            self.config.address.is_none(),
                            STOP_VERIFY,
                        )
                        .await
                    }
                    None => false,
                };
                if ended {
                    self.stopping = true;
                    self.farewell(NoticeLevel::Info, format!("stopped the daemon (pid {pid})"));
                } else {
                    self.farewell(
                        NoticeLevel::Warn,
                        format!(
                            "the daemon (pid {pid}) may still be running: the connection dropped before it confirmed the shutdown; rkat-tui --list shows it, /daemon stop from an attached rkat-tui or `kill {pid}` stops it"
                        ),
                    );
                }
            }
            // A daemon that refused `rkat/shutdown` (one without it):
            // SIGTERM, as `/daemon stop`.
            (Departure::Terminate, Err(err)) => {
                tracing::info!(error = %err, "rkat/shutdown failed; sending SIGTERM");
                match self.sigterm_attached().await {
                    Ok(pid) => self.farewell(
                        NoticeLevel::Info,
                        format!("asked the daemon (pid {pid}) to shut down"),
                    ),
                    Err(why) => self.farewell(
                        NoticeLevel::Error,
                        format!("the daemon was not stopped: {why}"),
                    ),
                }
            }
        }
    }
}

/// The orchestrator as the [`Workbench`] reaches it: system context over
/// the connection this client holds (`None`: not attached now).
struct Linked(Option<Arc<Connection>>);

impl AgentContext for Linked {
    type Error = CallFailure;

    fn append_system_context(
        &self,
        text: String,
        key: String,
    ) -> impl std::future::Future<Output = Result<(), CallFailure>> + Send {
        let connection = self.0.clone();
        async move {
            let connection = connection.ok_or(CallFailure::NotAttached)?;
            bounded(
                CALL_TIMEOUT,
                remote::append_system_context(&connection, &text, &key),
            )
            .await
        }
    }
}

/// The endpoint and model the status bar names, as the picker lists them
/// first.
fn current<'a>(endpoint: Option<&'a str>, model: Option<&'a (String, String)>) -> Current<'a> {
    Current {
        endpoint: endpoint.unwrap_or(""),
        model: model.map_or("", |(_, model)| model.as_str()),
    }
}

/// Whether the daemon `endpoint` names ended within `wait`. Evidence that
/// it did: no endpoint file at `files` names it any more (each is gone,
/// or names another daemon), its process is gone, or a connect to its
/// address is refused outright ([`Probe::Refused`]). A file that cannot
/// be read or parsed is no evidence either way, and neither is a connect
/// that timed out or failed otherwise ([`Probe::Unknown`]: a loaded host
/// or a local socket error says nothing about the daemon). `direct`: the
/// daemon was reached at its own endpoint, not by address
/// (`--attach=HOST:PORT`). Through a tunnel neither its pid (perhaps
/// another machine's) nor the local port (the forward may have exited
/// while the daemon runs on) says anything about the daemon, so only the
/// endpoint files count there. No files is no evidence.
pub(crate) async fn daemon_ended(
    files: &[PathBuf],
    endpoint: &Endpoint,
    direct: bool,
    wait: Duration,
) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        if no_file_names(files, &endpoint.token)
            || (direct
                && (crate::daemon::list::pid_gone(endpoint.pid)
                    || probe_address(endpoint.address) == Probe::Refused))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whether every file of `files` is gone or names a daemon with another
/// token (an unreadable or malformed file counts as possibly naming it).
fn no_file_names(files: &[PathBuf], token: &str) -> bool {
    !files.is_empty()
        && files.iter().all(|file| match std::fs::metadata(file) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
            Ok(_) => Endpoint::read(file).is_ok_and(|now| now.token != token),
        })
}

/// What a connect to a daemon's address says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// Something accepted.
    Accepts,
    /// Refused outright (nothing listens): evidence the daemon stopped.
    Refused,
    /// Timed out or failed otherwise: no evidence either way.
    Unknown,
}

/// Probe the loopback `address` now (a connect there is refused at once
/// when nothing listens).
fn probe_address(address: std::net::SocketAddr) -> Probe {
    match std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)) {
        Ok(_) => Probe::Accepts,
        Err(err) => classify(&err),
    }
}

/// A failed connect: a refusal (or a reset, which a loopback connect to a
/// port nobody listens on can report) is evidence of a stop; anything
/// else (a timeout, a local socket error) is not.
#[must_use]
pub fn classify(err: &std::io::Error) -> Probe {
    match err.kind() {
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset => {
            Probe::Refused
        }
        _ => Probe::Unknown,
    }
}

/// How an attached `/quit` leaves, from the daemon's status (`None`: it
/// could not say): stop the daemon when this client started it, nobody
/// detached and nobody else is attached; leave it to the other clients
/// when there are any; else ask.
#[must_use]
pub fn quit_decision(status: Option<&protocol::Status>) -> QuitDecision {
    let Some(status) = status else {
        return QuitDecision::Ask {
            reason: "it could not say who started it".to_string(),
            running: false,
        };
    };
    if status.clients > 1 {
        return QuitDecision::Leave;
    }
    let reason = if status.detached {
        "a client detached from it earlier"
    } else if status.owner {
        return QuitDecision::Terminate;
    } else {
        "this rkat-tui did not start it"
    };
    QuitDecision::Ask {
        reason: reason.to_string(),
        running: status.running,
    }
}

/// The pid `/daemon stop` signals: the endpoint file's, when its address
/// still answers and is the one this client is attached to.
///
/// # Errors
///
/// Why nothing is signalled.
pub fn stop_target(
    liveness: &launch::Liveness,
    connected: Option<std::net::SocketAddr>,
) -> Result<u32, String> {
    match (liveness, connected) {
        (launch::Liveness::Live(endpoint), Some(address)) if endpoint.address == address => {
            Ok(endpoint.pid)
        }
        (launch::Liveness::Live(endpoint), _) => Err(format!(
            "the endpoint file names a daemon at {} this client is not attached to; not stopping it",
            endpoint.address
        )),
        (launch::Liveness::Stale(_) | launch::Liveness::Absent, _) => {
            Err("no daemon answers for this workspace".to_string())
        }
    }
}

#[cfg(unix)]
fn terminate(pid: u32) -> Result<(), String> {
    let pid = i32::try_from(pid).map_err(|_| format!("pid {pid} is out of range"))?;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .map_err(|err| err.to_string())
}

#[cfg(not(unix))]
fn terminate(_pid: u32) -> Result<(), String> {
    Err("stopping the daemon is supported on Unix only".to_string())
}

/// `/model <reference>` in attach mode: resolved against this process's
/// endpoints and their fetched lists as in-process
/// ([`parse_model_ref`]); a configured endpoint's switch carries its auth
/// binding, a built-in one's clears it.
///
/// # Errors
///
/// [`ModelRefError`] when the reference picks no single endpoint.
pub fn model_switch(
    reference: &str,
    endpoints: &EndpointRegistry,
    models: Option<&ModelCache>,
    current: Option<&str>,
) -> Result<ModelSwitch, ModelRefError> {
    let index = models.map(ModelCache::index).unwrap_or_default();
    let target = parse_model_ref(reference, endpoints.as_slice(), &index, current)?;
    let endpoint =
        endpoints
            .get(&target.endpoint)
            .ok_or_else(|| ModelRefError::UnknownEndpoint {
                name: target.endpoint.clone(),
                known: endpoints.names(),
            })?;
    Ok(ModelSwitch {
        model: target.model,
        provider: Some(endpoint.dialect.as_str().to_string()),
        auth_binding: endpoint.binding(),
        endpoint: endpoint.name.clone(),
    })
}

/// Every durable event of `session` after `after`, and the last sequence.
async fn replay_to_end(
    connection: &Connection,
    session: &SessionId,
    after: u64,
) -> Result<(Vec<EventEnvelope<AgentEvent>>, u64), ClientError> {
    let mut events = Vec::new();
    let mut after = after;
    loop {
        let (page, more) = remote::replay(connection, session, after).await?;
        after = page.last().map_or(after, |envelope| envelope.seq);
        let empty = page.is_empty();
        events.extend(page);
        if !more || empty {
            return Ok((events, after));
        }
    }
}

/// The context of the last model call `events` report (the gauge's
/// reading), `None` when no turn reported its usage.
fn last_context(events: &[EventEnvelope<AgentEvent>]) -> Option<u64> {
    events
        .iter()
        .rev()
        .find_map(|envelope| match &envelope.payload {
            AgentEvent::TurnCompleted {
                usage: Some(usage), ..
            } => Some(crate::ui::context_tokens(usage)),
            _ => None,
        })
}

/// How many events before the mark the first look for the last turn end
/// reads, and how many it reads at most (the window doubles each time).
const CONTEXT_WINDOW: u64 = 256;
const CONTEXT_SEARCH_MAX: u64 = 4096;

/// The context of the last model call among `session`'s events up to
/// `before` (the gauge's seed on attach), looking back in doubling windows,
/// [`CONTEXT_SEARCH_MAX`] events at most. `None` when there is none, or it
/// cannot be read.
async fn last_context_before(
    connection: &Connection,
    session: &SessionId,
    before: u64,
) -> Option<u64> {
    let mut end = before;
    let mut window = CONTEXT_WINDOW;
    let mut searched = 0;
    while end > 0 && searched < CONTEXT_SEARCH_MAX {
        let start = end.saturating_sub(window);
        let mut after = start;
        let mut events = Vec::new();
        loop {
            let (page, more) = remote::replay(connection, session, after).await.ok()?;
            after = page.last().map_or(after, |envelope| envelope.seq);
            let empty = page.is_empty();
            events.extend(page.into_iter().filter(|envelope| envelope.seq <= end));
            if !more || empty || after >= end {
                break;
            }
        }
        if let Some(context) = last_context(&events) {
            return Some(context);
        }
        searched += end - start;
        end = start;
        window = window.saturating_mul(2);
    }
    None
}

/// This client's `/new` (`target` `None`, `fresh`) or `/resume`: ask the
/// daemon to switch, then read the conversation it switched to.
async fn switch_and_adopt(
    connection: &Connection,
    stream: &str,
    target: Option<&SessionId>,
    fresh: bool,
) -> Report {
    let switched = bounded(SWITCH_TIMEOUT, remote::switch(connection, target)).await;
    let (result, warning) = match switched {
        Ok((session, warning)) => (adopt(connection, stream, session).await, warning),
        Err(err) => (Err(err.to_string()), None),
    };
    Report::Switched {
        result,
        own: Some(fresh),
        warning,
    }
}

/// Follow the orchestrator's conversation `session` on `connection`: its
/// stream opened again (the old one follows the retired session), its last
/// sequence (what is shown starts after it) and its committed transcript.
async fn adopt(
    connection: &Connection,
    old_stream: &str,
    session: SessionId,
) -> Result<Adopted, String> {
    tokio::time::timeout(SWITCH_TIMEOUT, async {
        let stream = remote::reopen_stream(connection, old_stream).await?;
        let snapshot = remote::snapshot(connection, &session).await?;
        let messages = remote::transcript(connection, &session).await?;
        let usage = remote::usage(connection, &session).await.ok();
        let context = last_context_before(connection, &session, snapshot.latest).await;
        Ok::<_, ClientError>(Adopted {
            blocks: crate::runner::blocks_from_messages(&messages),
            session,
            stream,
            last_seq: snapshot.latest,
            identity: Some((snapshot.model, snapshot.provider)),
            usage,
            context,
        })
    })
    .await
    .map_err(|_| "timed out reading the new conversation".to_string())?
    .map_err(|err| err.to_string())
}

/// The endpoint the first connect attaches to: a live one, or (auto mode)
/// the one a started daemon writes.
async fn first_endpoint(
    files: &[PathBuf],
    spawn: Option<&SpawnPlan>,
    start_token: Option<&str>,
    ui_tx: &mpsc::UnboundedSender<Msg>,
) -> Result<Endpoint, ClientError> {
    let mode = if spawn.is_some() {
        AttachMode::Auto
    } else {
        AttachMode::Existing
    };
    let decision = launch::decide(launch::probe_any(files).await, mode);
    match (decision, spawn) {
        (Decision::Attach(endpoint), _) => Ok(endpoint),
        (Decision::Start, Some(plan)) => {
            let _ = ui_tx.send(Msg::Status(StatusUpdate::Attach(Some(STARTING.into()))));
            let (pid, mut exited) = launch::spawn_daemon(
                &plan.program,
                &launch::argv_for(&plan.argv, files),
                &plan.log,
                start_token,
            )?;
            let _ = ui_tx.send(Msg::Notice {
                level: NoticeLevel::Info,
                text: format!(
                    "started rkat-tui-mob (pid {pid}; output in {})",
                    plan.log.display()
                ),
            });
            launch::wait_for_endpoint(files, launch::START_TIMEOUT, &mut exited, &plan.log).await
        }
        (Decision::Start | Decision::Refuse, _) => Err(ClientError::NoDaemon {
            file: files.first().cloned().unwrap_or_default(),
        }),
    }
}

/// Start the daemon again after it died (its endpoint file is stale) and
/// wait for its endpoint.
async fn restart(
    files: &[PathBuf],
    plan: &SpawnPlan,
    ui_tx: &mpsc::UnboundedSender<Msg>,
) -> Result<Endpoint, ClientError> {
    let _ = ui_tx.send(Msg::Status(StatusUpdate::Attach(Some(STARTING.into()))));
    // No start token: a daemon that died lost what it knew about its
    // clients, so the restarted one has no owner and `/quit` asks.
    let (pid, mut exited) = launch::spawn_daemon(
        &plan.program,
        &launch::argv_for(&plan.argv, files),
        &plan.log,
        None,
    )?;
    let _ = ui_tx.send(Msg::Notice {
        level: NoticeLevel::Warn,
        text: format!(
            "the daemon died (its endpoint file is stale); restarted rkat-tui-mob (pid {pid}; output in {})",
            plan.log.display()
        ),
    });
    launch::wait_for_endpoint(files, launch::START_TIMEOUT, &mut exited, &plan.log).await
}

/// Attach, find the orchestrator's session and open its stream: resumed
/// from the saved cursor when it names this daemon's stream of the session
/// (the events since come on the stream), else live after rendering the
/// conversation from its start.
async fn establish(
    endpoint: Endpoint,
    saved: Option<&Cursor>,
    start_token: Option<&str>,
) -> Result<Established, ClientError> {
    let (connection, mut notifications) =
        Connection::attach_as(&endpoint, ATTACH_TIMEOUT, start_token, false).await?;
    tokio::time::timeout(CALL_TIMEOUT, async {
        let capabilities = connection
            .call(protocol::engine::INITIALIZE, serde_json::json!({}))
            .await?;
        if !pushes_decisions(&capabilities) {
            return Err(ClientError::DaemonTooOld { pid: endpoint.pid });
        }
        let session = remote::session_id(&connection).await?;
        let id = session.to_string();
        let resume = saved
            .and_then(|cursor| cursor.resumable(&id, &super::catchup::daemon_id(&endpoint.token)));
        let (stream, mark, reset, context) = match resume {
            Some((epoch, seq)) => {
                let stream = remote::open_stream(&connection, Some((&epoch, seq))).await?;
                let mark = Mark::resume(&id, saved);
                // The gauge's seed: the stream replays only what came after
                // the mark.
                let context = last_context_before(&connection, &session, seq).await;
                (stream, mark, None, context)
            }
            None => {
                // Live first, so nothing falls between the stream and the
                // render; the mark drops what both deliver. The snapshot's
                // cursor is where the render ends.
                let stream = remote::open_stream(&connection, None).await?;
                let latest = remote::snapshot(&connection, &session).await?.latest;
                let messages = remote::transcript(&connection, &session).await?;
                let context = last_context_before(&connection, &session, latest).await;
                (
                    stream,
                    Mark::rendered(&id, latest),
                    Some(crate::runner::blocks_from_messages(&messages)),
                    context,
                )
            }
        };
        let identity = remote::identity(&connection, &session).await.ok();
        let usage = remote::usage(&connection, &session).await.ok();
        // Every notification read before the usage answer: a turn that
        // ended before the usage read is in the totals; its live end must
        // not be added on top.
        let mut queued = Vec::new();
        while let Ok(note) = notifications.try_recv() {
            queued.push(note);
        }
        Ok(Established {
            link: Link {
                connection: Arc::new(connection),
                notifications,
                stream,
                session,
                endpoint,
            },
            mark,
            reset,
            identity,
            usage,
            queued,
            context,
        })
    })
    .await
    .map_err(|_| ClientError::Timeout("reading the orchestrator's state"))?
}

/// Whether a daemon's `initialize` answer lists the decision methods
/// (`decision/notice`, `decision/decide`): its approval requests reach this
/// client only as pushed decisions.
fn pushes_decisions(capabilities: &serde_json::Value) -> bool {
    let methods = capabilities
        .get("methods")
        .and_then(serde_json::Value::as_array);
    crate::daemon::serve::DECISION_METHODS.iter().all(|name| {
        methods.is_some_and(|methods| methods.iter().any(|method| method.as_str() == Some(name)))
    })
}

/// Events only a running run publishes (its start aside).
fn joins_a_run(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::TurnStarted { .. }
            | AgentEvent::TextDelta { .. }
            | AgentEvent::TextComplete { .. }
            | AgentEvent::ToolExecutionStarted { .. }
            | AgentEvent::ToolExecutionCompleted { .. }
            | AgentEvent::TurnCompleted { .. }
            | AgentEvent::RunCompleted { .. }
            | AgentEvent::RunFailed { .. }
    )
}

async fn next_notification(link: &mut Option<Link>) -> Option<Notification> {
    match link {
        Some(link) => link.notifications.recv().await,
        None => std::future::pending().await,
    }
}

async fn join<T>(task: &mut Option<JoinHandle<T>>) -> Result<T, tokio::task::JoinError> {
    match task {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

    /// A runner on a workspace in `dir`, never connected.
    fn runner(dir: &std::path::Path) -> (AttachRunner, mpsc::UnboundedReceiver<Msg>) {
        let root = dir.to_str().unwrap();
        let cfg = format!("{root}/.cfg");
        let args = <crate::cli::Args as clap::Parser>::try_parse_from([
            "rkat-tui",
            "-w",
            root,
            "--user-config-root",
            root,
            "--config-dir",
            &cfg,
        ])
        .unwrap();
        let scope = crate::bootstrap::resolve_scope(&args).unwrap();
        let config = AttachConfig {
            endpoint_files: vec![dir.join("mob.endpoint")],
            cursor: CursorFile::of_workspace(&scope.context_root),
            config: None,
            spawn: None,
            address: None,
            remote: None,
            start: super::super::StartAt::Current,
            start_token: None,
            user_shell: Err("none".into()),
            endpoints: EndpointRegistry::from_endpoints(Vec::new()),
            lister: crate::models::live_lister(),
            ui: crate::config::UiSettings::default(),
            compaction: crate::ui::CompactionPolicy::default(),
            pricing: crate::pricing::PricingSetup::default(),
            scope,
        };
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let (_effects, effect_rx) = mpsc::unbounded_channel();
        (AttachRunner::new(config, ui_tx, effect_rx), ui_rx)
    }

    /// A link over an in-memory pipe whose daemon side only answers the
    /// attach line.
    async fn link(stream: &str, session: SessionId) -> Link {
        link_held(stream, session, true).await
    }

    /// [`link`]; the daemon side drops the connection right after the
    /// attach unless `held`.
    async fn link_held(stream: &str, session: SessionId, held: bool) -> Link {
        let (client, server) = tokio::io::duplex(1 << 16);
        let (server_read, mut server_write) = tokio::io::split(server);
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(server_read);
            let mut line = String::new();
            lines.read_line(&mut line).await.unwrap();
            let attach: serde_json::Value = serde_json::from_str(&line).unwrap();
            let answer = serde_json::json!({"jsonrpc": "2.0", "id": attach["id"], "result": {"attached": true}});
            server_write
                .write_all(format!("{answer}\n").as_bytes())
                .await
                .unwrap();
            // Held open: the link stays up.
            if held {
                std::future::pending::<()>().await;
            }
        });
        let (read, write) = tokio::io::split(client);
        let address: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
        let (connection, notifications) = Connection::attach_io(
            read,
            write,
            protocol::AttachParams {
                token: "t".into(),
                ..protocol::AttachParams::default()
            },
            address,
            ATTACH_TIMEOUT,
        )
        .await
        .unwrap();
        Link {
            connection: Arc::new(connection),
            notifications,
            stream: stream.to_string(),
            session,
            endpoint: Endpoint {
                address,
                token: "t".into(),
                pid: 1,
                workspace: None,
                started_at: None,
            },
        }
    }

    fn switched(session: &SessionId, stream: &str) -> Report {
        Report::Switched {
            result: Ok(Adopted {
                session: session.clone(),
                stream: stream.to_string(),
                blocks: Vec::new(),
                last_seq: 3,
                identity: None,
                usage: None,
                context: None,
            }),
            own: None,
            warning: None,
        }
    }

    /// The finding this guards: a switch read on a connection lost before
    /// the reattach installed that connection's stream id, and every
    /// orchestrator event after was dropped as another stream's.
    #[tokio::test]
    async fn a_switch_read_on_a_lost_connection_is_not_installed_after_the_reattach() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut ui_rx) = runner(dir.path());
        let live = SessionId::new();
        runner.link = Some(link("live", live.clone()).await);
        runner.link_epoch = 2;
        let other = SessionId::new();
        runner.switching = true;
        runner.on_report(1, switched(&other, "dead"));
        let link_now = runner.link.as_ref().unwrap();
        assert_eq!(link_now.stream, "live");
        assert_eq!(link_now.session, live);
        assert!(runner.switching, "a switch started since still runs");
        while let Ok(msg) = ui_rx.try_recv() {
            assert!(!matches!(msg, Msg::ResetTranscript(_)), "{msg:?}");
        }
        // The same report of the current link is shown.
        runner.on_report(2, switched(&other, "next"));
        let link_now = runner.link.as_ref().unwrap();
        assert_eq!(link_now.stream, "next");
        assert_eq!(link_now.session, other);
        assert!(!runner.switching);
        let mut reset = false;
        while let Ok(msg) = ui_rx.try_recv() {
            reset |= matches!(msg, Msg::ResetTranscript(_));
        }
        assert!(reset);
    }

    /// The finding this guards: a shutdown whose connection dropped before
    /// the daemon answered was reported as "stopped the daemon" although
    /// the daemon never handled it and kept running.
    #[tokio::test(start_paused = true)]
    async fn a_shutdown_without_an_answer_is_reported_stopped_only_once_the_daemon_ended() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mob.endpoint");
        // The captured daemon's address still accepts and its pid (1) is
        // alive, unless a case says otherwise.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_address = closed.local_addr().unwrap();
        drop(closed);
        // (case, reported stopped)
        for (case, ended) in [
            ("named", false),
            ("removed", true),
            ("malformed", false),
            ("malformed, address closed", true),
        ] {
            let (mut runner, mut ui_rx) = runner(dir.path());
            let mut link = link_held("s", SessionId::new(), false).await;
            link.endpoint.address = if case.ends_with("address closed") {
                closed_address
            } else {
                listener.local_addr().unwrap()
            };
            link.endpoint.write(&file).unwrap();
            match case {
                "removed" => std::fs::remove_file(&file).unwrap(),
                c if c.starts_with("malformed") => std::fs::write(&file, "{not json").unwrap(),
                _ => {}
            }
            runner.link = Some(link);
            runner.depart(Departure::Terminate).await;
            let mut notices = Vec::new();
            while let Ok(msg) = ui_rx.try_recv() {
                if let Msg::Notice { text, .. } = msg {
                    notices.push(text);
                }
            }
            let said = notices.join("\n");
            assert_eq!(said.contains("stopped the daemon"), ended, "{case}: {said}");
            assert_eq!(
                !said.contains("may still be running"),
                ended,
                "{case}: {said}"
            );
            assert_eq!(runner.stopping, ended, "{case}");
        }
    }

    /// The finding this guards: through a tunnel a refused connect to the
    /// local port was taken as the daemon's end, so an `ssh -L` forward
    /// that exited during an unanswered shutdown was reported as a stopped
    /// daemon while the remote daemon ran on.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_tunnel_leaves_an_unanswered_shutdown_unconfirmed() {
        let dir = tempfile::tempdir().unwrap();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_address = closed.local_addr().unwrap();
        drop(closed);
        // Remote (no endpoint files) and local by address (its file still
        // names it): the forward's port refuses, the pid (1) is alive.
        for remote in [true, false] {
            let (mut runner, mut ui_rx) = runner(dir.path());
            let mut link = link_held("s", SessionId::new(), false).await;
            link.endpoint.address = closed_address;
            link.endpoint.pid = 1;
            if remote {
                runner.config.endpoint_files.clear();
                runner.config.remote = Some("the daemon runs on devbox".into());
            } else {
                link.endpoint
                    .write(&dir.path().join("mob.endpoint"))
                    .unwrap();
            }
            runner.config.address = Some(link.endpoint.clone());
            runner.link = Some(link);
            runner.depart(Departure::Terminate).await;
            let said = std::iter::from_fn(|| ui_rx.try_recv().ok())
                .filter_map(|msg| match msg {
                    Msg::Notice { text, .. } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(said.contains("may still be running"), "{remote}: {said}");
            assert!(!said.contains("stopped the daemon"), "{remote}: {said}");
            assert!(!runner.stopping, "{remote}");
        }
        // Its endpoint file gone is the daemon's own evidence, by address too.
        let (mut runner, mut ui_rx) = runner(dir.path());
        let mut link = link_held("s", SessionId::new(), false).await;
        link.endpoint.address = closed_address;
        std::fs::remove_file(dir.path().join("mob.endpoint")).unwrap();
        runner.config.address = Some(link.endpoint.clone());
        runner.link = Some(link);
        runner.depart(Departure::Terminate).await;
        let said = std::iter::from_fn(|| ui_rx.try_recv().ok())
            .filter_map(|msg| match msg {
                Msg::Notice { text, .. } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(said.contains("stopped the daemon"), "{said}");
    }

    /// A refused `/resume` ends the switch; nothing else that reports a
    /// notice does.
    #[tokio::test]
    async fn only_a_refused_switch_ends_the_switch() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut ui_rx) = runner(dir.path());
        runner.link = Some(link("live", SessionId::new()).await);
        runner.switching = true;
        runner.on_report(0, Report::Interrupt("x".into()));
        assert!(runner.switching);
        // One of a lost connection says why, and leaves a later switch be.
        runner.link_epoch = 1;
        runner.on_report(
            0,
            Report::SwitchRefused(NoticeLevel::Info, "already in this conversation".into()),
        );
        assert!(runner.switching);
        runner.on_report(
            1,
            Report::SwitchRefused(NoticeLevel::Info, "already in this conversation".into()),
        );
        assert!(!runner.switching);
        let notices = std::iter::from_fn(|| ui_rx.try_recv().ok())
            .filter(|msg| matches!(msg, Msg::Notice { text, .. } if text == "already in this conversation"))
            .count();
        assert_eq!(notices, 2);
    }

    #[test]
    fn model_references_resolve_on_this_processs_endpoints() {
        let mut endpoints = EndpointRegistry::from_endpoints(Vec::new());
        endpoints.add_keyless_builtins();
        let switch = model_switch("anthropic:claude-opus-4-8", &endpoints, None, None).unwrap();
        assert_eq!(switch.model, "claude-opus-4-8");
        assert_eq!(switch.provider.as_deref(), Some("anthropic"));
        assert_eq!(switch.auth_binding, None);
        assert_eq!(switch.endpoint, "anthropic");
        // A catalogued bare id finds its endpoint.
        let switch = model_switch("claude-sonnet-4-6", &endpoints, None, None).unwrap();
        assert_eq!(switch.endpoint, "anthropic");
        assert!(model_switch("my-alias", &endpoints, None, None).is_err());
        assert!(model_switch("litellm:x", &endpoints, None, None).is_err());
    }

    #[test]
    fn a_daemon_without_the_decision_methods_is_too_old() {
        let current =
            serde_json::json!({"methods": ["mob/send", "decision/notice", "decision/decide"]});
        assert!(pushes_decisions(&current));
        for old in [
            serde_json::json!({"methods": ["mob/send", "approval/list", "approval/decide"]}),
            serde_json::json!({"methods": ["decision/notice"]}),
            serde_json::json!({}),
        ] {
            assert!(!pushes_decisions(&old), "{old}");
        }
        let err = ClientError::DaemonTooOld { pid: 42 }.to_string();
        assert!(err.contains("too old") && err.contains("restart"), "{err}");
    }

    #[test]
    fn daemon_stop_signals_only_the_daemon_this_client_is_attached_to() {
        let at: std::net::SocketAddr = "127.0.0.1:4000".parse().unwrap();
        let live = launch::Liveness::Live(Endpoint {
            address: at,
            token: "t".into(),
            pid: 42,
            workspace: None,
            started_at: None,
        });
        assert_eq!(stop_target(&live, Some(at)), Ok(42));
        assert!(stop_target(&live, Some("127.0.0.1:4001".parse().unwrap())).is_err());
        assert!(stop_target(&live, None).is_err());
        assert!(stop_target(&launch::Liveness::Absent, Some(at)).is_err());
    }

    #[test]
    fn orphans_are_counted_bounded_and_released() {
        let mut orphans = Orphans::default();
        for n in 0..ORPHANS_MAX + 5 {
            orphans.hold(if n % 2 == 0 { "a" } else { "b" }.to_string(), n);
        }
        assert_eq!(orphans.held, ORPHANS_MAX);
        assert!(orphans.warned, "warned once");
        let a = orphans.take("a");
        assert_eq!(a.len(), ORPHANS_MAX / 2);
        assert_eq!(a.first(), Some(&0));
        assert_eq!(orphans.held, ORPHANS_MAX / 2);
        orphans.hold("c".into(), 1);
        assert_eq!(orphans.take("c"), vec![1]);
        orphans.clear();
        assert_eq!((orphans.held, orphans.warned), (0, false));
        assert!(orphans.take("b").is_empty());
    }

    /// Only a refusal proves a stop: a connect that timed out or failed
    /// otherwise leaves the question to the pid and the endpoint file.
    #[test]
    fn only_a_refused_connect_counts_as_a_stopped_daemon() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            classify(&Error::from(ErrorKind::ConnectionRefused)),
            Probe::Refused
        );
        assert_eq!(
            classify(&Error::from(ErrorKind::ConnectionReset)),
            Probe::Refused
        );
        for kind in [
            ErrorKind::TimedOut,
            ErrorKind::WouldBlock,
            ErrorKind::PermissionDenied,
            ErrorKind::AddrNotAvailable,
            ErrorKind::Other,
        ] {
            assert_eq!(classify(&Error::from(kind)), Probe::Unknown, "{kind:?}");
        }
    }

    #[test]
    fn a_turn_keeps_its_worst_end() {
        let mut turn = Turn::default();
        turn.record(Ok(TurnSummary::default()));
        turn.record(Err(TurnFailure::Interrupted));
        turn.record(Ok(TurnSummary::default()));
        assert_eq!(turn.result, Some(Err(TurnFailure::Interrupted)));
        turn.record(Err(TurnFailure::Failed("x".into())));
        turn.record(Err(TurnFailure::Interrupted));
        assert_eq!(turn.result, Some(Err(TurnFailure::Failed("x".into()))));
    }
}
