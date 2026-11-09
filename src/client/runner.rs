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
use crate::delivery::{PendingId, SteerFailure};
use crate::endpoints::{
    EndpointRegistry, ModelRefError, WireDialect, needs_lookup, parse_model_ref,
};
use crate::models::{FetchResults, ModelCache, ModelGroup, fetch_warning, with_current};
use crate::ui::{
    Block, CompactFailure, Effect, EscalationAnswer, EscalationId, MemoryTarget, Msg, NoticeLevel,
    RuleEffect, ShellOutcome, ShellRunId, StatusUpdate, TurnFailure, TurnOutcome, TurnSummary,
};
use crate::user_shell::UserShell;

/// The status-bar segment while the first connection is made.
pub const CONNECTING: &str = "connecting";
/// The status-bar segment while a started daemon comes up.
pub const STARTING: &str = "daemon starting";
/// The status-bar segment while a dropped connection is re-made.
pub const RECONNECTING: &str = "reconnecting";

/// What attach mode answers for what the daemon cannot do yet.
pub const NOT_AVAILABLE: &str = "not available in attach mode yet";

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

/// The notice shown on every attach.
pub const EXPERIMENTAL_NOTICE: &str = "attach mode is experimental: the agent runs in this workspace's rkat-tui-mob daemon, behind the same approval gate (its approval requests show here, and in every other attached client; the first answer counts). /quit detaches and leaves the daemon running; /daemon stop shuts it down.";

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
    /// The daemon's endpoint file.
    pub endpoint_file: PathBuf,
    /// The workspace's attach cursor.
    pub cursor: CursorFile,
    /// The realm config (context windows); `None`: unknown windows.
    pub config: Option<meerkat_core::Config>,
    /// How to start a missing daemon (`None`: `--attach=existing`).
    pub spawn: Option<SpawnPlan>,
    /// The user's `!` shell (this process's own, as in-process), or why
    /// there is none.
    pub user_shell: Result<Arc<UserShell>, String>,
    /// This process's LLM endpoints (the model picker, `/model`).
    pub endpoints: crate::endpoints::EndpointRegistry,
    /// Fetches the endpoints' model lists (tests list nothing).
    pub lister: crate::models::ModelLister,
    /// The `[ui]` settings of the `tui.toml` layers.
    pub ui: crate::approval::rules::UiSettings,
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
            .field("endpoint_file", &self.endpoint_file)
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
    /// A `mob/turn_start` (the model switch) failed.
    TurnStart(ClientError),
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
    /// A conversation switch of this client's did not start (the
    /// conversations could not be listed, the reference resolved to none
    /// or to the current one): why. Ends the switch.
    SwitchRefused(NoticeLevel, String),
    /// The endpoints' model lists.
    ModelLists(FetchResults),
    /// The community price list, refreshed.
    Community(Result<crate::pricing::CommunityPrices, crate::pricing::PricingError>),
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
        let rank = |r: &Result<TurnSummary, TurnFailure>| match r {
            Ok(_) => 0,
            Err(TurnFailure::Unknown(_) | TurnFailure::Pending { .. }) => 1,
            Err(TurnFailure::Interrupted) => 2,
            Err(TurnFailure::Failed(_)) => 3,
        };
        self.result = match self.result.take() {
            Some(current) if rank(&current) > rank(&end) => Some(current),
            _ => Some(end),
        };
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
    /// `/model`: the model the next prompt switches to.
    pending_model: Option<ModelSwitch>,
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
    /// The running `!` command.
    shell_task: Option<(ShellRunId, JoinHandle<()>)>,
    /// The orchestrator's helpers.
    followers: Followers,
    /// Events of streams opened but not registered yet, by stream id.
    orphans: std::collections::HashMap<String, Vec<meerkat_mob::AttributedEvent>>,
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
    /// The endpoints' model lists, once fetched.
    models: Option<ModelCache>,
    /// A model-list fetch runs.
    fetching_models: bool,
    /// The picker opens when the lists arrive.
    picker_waiting: bool,
    /// A `/model <ref>` waiting for the lists.
    pending_set: Option<String>,
    /// The file `/memory` opened and its bytes before.
    memory_before: Option<(PathBuf, Option<Vec<u8>>)>,
    /// The cost readout's prices.
    prices: Arc<crate::pricing::PriceTable>,
    /// The messages the UI still held when the user quit
    /// ([`Effect::SaveUnsent`]).
    held_unsent: Vec<(Option<PendingId>, String)>,
    /// The steers the daemon admitted: the agent has them, or gets them
    /// after this client leaves, so quitting saves none of them.
    admitted: Vec<PendingId>,
}

impl AttachRunner {
    /// A runner on `config`; it connects once [`AttachRunner::run`] runs.
    #[must_use]
    pub fn new(
        config: AttachConfig,
        ui_tx: mpsc::UnboundedSender<Msg>,
        effect_rx: mpsc::UnboundedReceiver<Effect>,
    ) -> Self {
        let (reports_tx, reports_rx) = mpsc::unbounded_channel();
        let prices = Arc::new(config.pricing.table.clone());
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
            pending_model: None,
            model: None,
            stopping: false,
            daemon_id_seen: None,
            approvals: Approvals::default(),
            shell_task: None,
            followers: Followers::default(),
            orphans: std::collections::HashMap::new(),
            discover_at: None,
            discover_until: None,
            discovering: false,
            session_check_at: None,
            checking_session: false,
            switching: false,
            last_switch: None,
            current_endpoint: None,
            models: None,
            fetching_models: false,
            picker_waiting: false,
            pending_set: None,
            memory_before: None,
            prices,
            held_unsent: Vec::new(),
            admitted: Vec::new(),
        }
    }

    /// Serve effects until the UI quits, then detach (the daemon and any
    /// running turn go on). Returns where the messages the daemon never
    /// got were saved, when there were any.
    pub async fn run(mut self) -> Option<crate::runner::SavedUnsent> {
        let realm_root = self.config.scope.realm_root();
        if let Some(text) = crate::runner::quit::unsent_notice(&realm_root) {
            self.notice(NoticeLevel::Warn, text);
        }
        self.push_prices();
        self.refresh_prices();
        self.status(CONNECTING);
        self.connect();
        loop {
            tokio::select! {
                effect = self.effect_rx.recv() => match effect {
                    None | Some(Effect::Quit) => break,
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
            }
        }
        self.save_cursor();
        if let Some((_, task)) = self.shell_task.take() {
            task.abort();
        }
        if let Some(task) = self.connecting.take() {
            task.abort();
        }
        crate::runner::quit::save_unsent(
            &realm_root,
            std::mem::take(&mut self.held_unsent),
            &self.admitted,
        )
    }

    /// Send the price table to the UI.
    fn push_prices(&self) {
        self.send(Msg::Status(StatusUpdate::Prices(Arc::clone(&self.prices))));
    }

    /// Refresh the community price list in the background when it is due.
    fn refresh_prices(&self) {
        let Some(url) = self.config.pricing.refresh_url().map(str::to_string) else {
            return;
        };
        let cache = self.config.pricing.cache_path.clone();
        let reports = self.reporter();
        tokio::spawn(async move {
            let fetched = crate::pricing::refresh_community(
                &url,
                cache.as_deref(),
                crate::pricing::FETCH_TIMEOUT,
            )
            .await;
            reports.send(Report::Community(fetched));
        });
    }

    fn send(&self, msg: Msg) {
        let _ = self.ui_tx.send(msg);
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
        let file = self.config.endpoint_file.clone();
        let saved = self
            .mark
            .as_ref()
            .map(|mark| mark.cursor(self.daemon_id_seen.as_deref()))
            .or_else(|| self.config.cursor.load());
        let ui_tx = self.ui_tx.clone();
        let task = if std::mem::take(&mut self.first_connect) {
            let spawn = self.config.spawn.clone();
            tokio::spawn(async move {
                let endpoint = first_endpoint(&file, spawn.as_ref(), &ui_tx).await?;
                establish(endpoint, saved.as_ref()).await
            })
        } else {
            let spawn = self.config.spawn.clone();
            tokio::spawn(async move {
                let mut wait = BACKOFF_FIRST;
                let mut restarted: Option<Instant> = None;
                loop {
                    match launch::probe(&file).await {
                        launch::Liveness::Live(endpoint) => {
                            match establish(endpoint, saved.as_ref()).await {
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
                                match restart(&file, plan, &ui_tx).await {
                                    Ok(endpoint) => match establish(endpoint, saved.as_ref()).await
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
                self.send(Msg::Status(StatusUpdate::Attach(Some("detached".into()))));
                self.notice(
                    NoticeLevel::Error,
                    format!("cannot attach to the workspace daemon: {err}"),
                );
                return;
            }
            Err(err) => {
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
        self.daemon_id_seen = Some(super::catchup::daemon_id(&link.endpoint.token));
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
            self.notice(NoticeLevel::Warn, EXPERIMENTAL_NOTICE);
        } else {
            self.notice(
                NoticeLevel::Info,
                format!("reattached to the daemon at {address}"),
            );
        }
        if let Some((model, provider)) = identity {
            self.followers.set_default_model(&model);
            if self.pending_model.is_none() {
                self.set_model_status(&provider, &model);
            }
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
        } else if self.orphans.values().map(Vec::len).sum::<usize>() < ORPHANS_MAX {
            // A stream whose open has not answered yet.
            self.orphans.entry(stream).or_default().push(attributed);
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
            AgentEvent::BoundaryAppendApplied { .. } => {
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
            Report::TurnStart(err) => {
                self.notice(
                    NoticeLevel::Error,
                    format!("the model switch failed: {err}"),
                );
                if self.turn.active && !self.turn.saw_run {
                    self.turn.record(Err(TurnFailure::Failed(err.to_string())));
                    self.finish_turn();
                }
            }
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
                    for event in self.orphans.remove(&stream).unwrap_or_default() {
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
            Report::ModelLists(results) => self.model_lists(results),
            Report::Community(Ok(community)) => {
                Arc::make_mut(&mut self.prices).set_community(community);
                self.push_prices();
            }
            Report::Community(Err(err)) => {
                tracing::warn!(error = %err, "could not fetch the community price list");
            }
        }
    }

    fn connection(&self) -> Option<Arc<Connection>> {
        self.link.as_ref().map(|link| Arc::clone(&link.connection))
    }

    async fn handle_effect(&mut self, effect: Effect) {
        match effect {
            Effect::StartTurn(text) => self.start_turn(&text).await,
            Effect::Steer { id, text } => self.steer(id, &text).await,
            Effect::Interrupt | Effect::InterruptAndSend(_) => self.interrupt(),
            Effect::RecallQueued => self.send(Msg::Recalled {
                recalled: Vec::new(),
                errors: vec![format!("taking back a sent message is {NOT_AVAILABLE}")],
            }),
            Effect::RunShell { id, command } => self.run_shell(id, command),
            Effect::CancelShell => self.cancel_shell(),
            Effect::SetModel(reference) => self.set_model(&reference),
            Effect::CompactNow => self.compact_now(),
            Effect::StatusReport => self.status_report(),
            Effect::ContextReport(header) => self.notice(
                NoticeLevel::Info,
                format!("{header}\n(what the context holds is {NOT_AVAILABLE})"),
            ),
            Effect::Export { path, text } => self.export(path.as_deref(), &text),
            Effect::DaemonStop => self.daemon_stop().await,
            Effect::NewSession => self.switch_conversation(None),
            Effect::ResumeSession(reference) => self.resume_session(reference),
            Effect::RequestResumePicker => self.show_resume_picker(),
            Effect::RequestRewindMenu | Effect::Rewind(_) => {
                self.notice(NoticeLevel::Warn, sessions::REWIND_UNAVAILABLE);
            }
            Effect::RequestModelPicker { refresh } => self.request_model_picker(refresh),
            Effect::Remember(text) => self.remember(&text).await,
            Effect::OpenMemory(target) => self.open_memory(target),
            Effect::MemoryEdited { target, path } => self.memory_edited(target, &path).await,
            Effect::CostSnapshot(snapshot) => self.save_cost(&snapshot),
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
            Effect::SaveUnsent(held) => self.held_unsent.extend(held),
            Effect::Quit
            | Effect::SetChecklistView(_)
            | Effect::Redraw
            | Effect::Suspend
            | Effect::ExternalEditor(_) => {}
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

    /// `!command`: the user's own shell, in this process (as in-process);
    /// its output goes to the orchestrator's context.
    fn run_shell(&mut self, id: ShellRunId, command: String) {
        self.cancel_shell();
        let shell = match &self.config.user_shell {
            Ok(shell) => Arc::clone(shell),
            Err(why) => {
                self.send(Msg::ShellFinished {
                    id,
                    outcome: ShellOutcome::Failed(why.clone()),
                });
                return;
            }
        };
        let connection = self.connection();
        let ui_tx = self.ui_tx.clone();
        let task = tokio::spawn(async move {
            let outcome = match shell.run(&command).await {
                Ok(run) => {
                    let text = crate::user_shell::context_text(&command, &run);
                    let context = match connection {
                        Some(connection) => remote::append_system_context(
                            &connection,
                            &text,
                            &crate::runner::shell::run_key(),
                        )
                        .await
                        .map_err(|err| err.to_string()),
                        None => Err("not attached to the daemon".to_string()),
                    };
                    ShellOutcome::Ran { run, context }
                }
                Err(err) => ShellOutcome::Failed(err.to_string()),
            };
            let _ = ui_tx.send(Msg::ShellFinished { id, outcome });
        });
        self.shell_task = Some((id, task));
    }

    fn cancel_shell(&mut self) {
        let Some((id, task)) = self.shell_task.take() else {
            return;
        };
        if task.is_finished() {
            return;
        }
        task.abort();
        self.send(Msg::ShellFinished {
            id,
            outcome: ShellOutcome::Cancelled,
        });
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
        let text = if text.contains('@') {
            crate::mentions::expand_mentions(text, &self.config.scope.project_root).text
        } else {
            text.to_string()
        };
        if let Some(switch) = self.pending_model.take() {
            let reports = self.reporter();
            tokio::spawn(async move {
                if let Err(err) = remote::turn_with_model(&connection, &text, &switch).await {
                    reports.send(Report::TurnStart(err));
                }
            });
            return;
        }
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
        let text = if text.contains('@') {
            crate::mentions::expand_mentions(text, &self.config.scope.project_root).text
        } else {
            text.to_string()
        };
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

    /// `/model <reference>` or a picker selection: the next prompt carries
    /// the switch (`mob/turn_start` with the model, provider and the
    /// endpoint's auth binding); the session keeps the model from then on.
    /// A bare id is resolved against this process's endpoints as
    /// in-process (their lists fetched first when several could serve it).
    fn set_model(&mut self, reference: &str) {
        match model_switch(
            reference,
            &self.config.endpoints,
            self.models.as_ref(),
            self.current_endpoint.as_deref(),
        ) {
            Ok(switch) => {
                self.notice(
                    NoticeLevel::Info,
                    format!(
                        "switching to {} on {} with your next message",
                        switch.model, switch.endpoint
                    ),
                );
                let dialect = switch.provider.as_deref().and_then(WireDialect::parse);
                self.show_model(&switch.endpoint, dialect, &switch.model);
                self.last_switch = Some(switch.clone());
                self.pending_model = Some(switch);
            }
            Err(_)
                if self.models.is_none()
                    && needs_lookup(reference, self.config.endpoints.as_slice()) =>
            {
                self.pending_set = Some(reference.to_string());
                self.fetch_models(true);
            }
            Err(err) => self.notice(NoticeLevel::Warn, err.to_string()),
        }
    }

    /// Fetch the endpoints' model lists in the background, once at a time.
    fn fetch_models(&mut self, announce: bool) {
        if self.fetching_models {
            return;
        }
        self.fetching_models = true;
        let endpoints: Vec<_> = self
            .config
            .endpoints
            .iter()
            .filter(|endpoint| endpoint.has_key())
            .cloned()
            .collect();
        let lister = Arc::clone(&self.config.lister);
        let reports = self.reporter();
        tokio::spawn(async move {
            reports.send(Report::ModelLists(lister(endpoints).await));
        });
        if announce {
            self.notice(NoticeLevel::Info, "fetching the model lists…");
        }
    }

    /// `/model` without an id: the picker, from the lists (fetched first).
    fn request_model_picker(&mut self, refresh: bool) {
        if refresh {
            self.models = None;
        }
        if self.models.is_some() || self.config.endpoints.is_empty() {
            self.send(Msg::ShowModelPicker(self.picker_groups()));
            return;
        }
        self.picker_waiting = true;
        self.fetch_models(true);
    }

    fn model_lists(&mut self, results: FetchResults) {
        self.fetching_models = false;
        let (cache, prices) = ModelCache::from_results(results);
        self.models = Some(cache);
        if !prices.is_empty() || !self.prices.is_empty() {
            Arc::make_mut(&mut self.prices).set_endpoint_prices(prices);
            self.push_prices();
        }
        if std::mem::take(&mut self.picker_waiting) {
            self.send(Msg::ShowModelPicker(self.picker_groups()));
        }
        if let Some(reference) = self.pending_set.take() {
            self.set_model(&reference);
        }
    }

    /// The picker: one group per endpoint, the current endpoint first with
    /// the current model first (as in-process).
    fn picker_groups(&self) -> Vec<ModelGroup> {
        let endpoints = &self.config.endpoints;
        let current_name = self.current_endpoint.as_deref().unwrap_or("");
        let current_model = self.model.as_ref().map_or("", |(_, model)| model.as_str());
        let current = endpoints.iter().filter(|e| e.name == current_name);
        let others = endpoints.iter().filter(|e| e.name != current_name);
        current
            .chain(others)
            .map(|endpoint| {
                let listed = self.models.as_ref().and_then(|m| m.get(&endpoint.name));
                let (models, warning) = match listed {
                    Some(Ok(list)) => (list.clone(), None),
                    Some(Err(err)) => (Vec::new(), Some(fetch_warning(err))),
                    None if !endpoint.has_key() => (
                        Vec::new(),
                        Some("model listing needs an API key the TUI can read".to_string()),
                    ),
                    None => (Vec::new(), None),
                };
                let current = if endpoint.name == current_name {
                    current_model
                } else {
                    ""
                };
                ModelGroup {
                    endpoint: endpoint.name.clone(),
                    dialect: endpoint.dialect,
                    base_url: endpoint.base_url.clone(),
                    models: with_current(current, models),
                    warning,
                }
            })
            .collect()
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
        if let Some((model, provider)) = identity {
            self.followers.set_default_model(&model);
            self.set_model_status(&provider, &model);
            // A fresh conversation starts on the daemon's start-up model:
            // this client's last `/model` carries over, as in-process.
            if own == Some(true)
                && let Some(switch) = self.last_switch.clone()
                && switch.model != model
            {
                self.notice(
                    NoticeLevel::Info,
                    format!("switching to {} with your next message", switch.model),
                );
                let dialect = switch.provider.as_deref().and_then(WireDialect::parse);
                self.show_model(&switch.endpoint, dialect, &switch.model);
                self.pending_model = Some(switch);
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

    /// `/remember <text>`: append it to the workspace `AGENTS.md` here and
    /// hand it to the orchestrator as system context (as in-process).
    async fn remember(&self, text: &str) {
        use crate::bootstrap::instructions::{
            append_remember_line, remember_line, remember_target, remembered_context,
        };
        let Some(line) = remember_line(text) else {
            self.notice(NoticeLevel::Warn, "nothing to remember");
            return;
        };
        let path = remember_target(&self.config.scope.context_root);
        if let Err(err) = append_remember_line(&path, &line) {
            self.notice(
                NoticeLevel::Error,
                format!("could not write {}: {err}", path.display()),
            );
            return;
        }
        let key = crate::runner::context::remember_key(&line);
        match self.push_context(&remembered_context(&line), &key).await {
            Ok(()) => self.notice(
                NoticeLevel::Info,
                format!(
                    "remembered in {}: the agent reads it from its next model call; it is fully applied (as part of AGENTS.md) from the next conversation",
                    path.display()
                ),
            ),
            Err(err) => self.notice(
                NoticeLevel::Warn,
                format!(
                    "remembered in {}, but the agent could not take it now ({err}); it applies from the next conversation",
                    path.display()
                ),
            ),
        }
    }

    /// Hand `text` to the orchestrator as system context under `key`.
    async fn push_context(&self, text: &str, key: &str) -> Result<(), CallFailure> {
        let connection = self.connection().ok_or(CallFailure::NotAttached)?;
        bounded(
            CALL_TIMEOUT,
            remote::append_system_context(&connection, text, key),
        )
        .await
    }

    /// `/memory`: open the instruction file here (as in-process).
    fn open_memory(&mut self, target: MemoryTarget) {
        let scope = &self.config.scope;
        let path = match crate::runner::context::memory_path(
            target,
            &scope.context_root,
            scope.global.as_ref(),
        ) {
            Ok(path) => path,
            Err(why) => {
                self.notice(NoticeLevel::Warn, why);
                return;
            }
        };
        if let Err(err) = crate::runner::context::prepare_memory_file(target, &path) {
            self.notice(
                NoticeLevel::Error,
                format!("could not prepare {}: {err}", path.display()),
            );
            return;
        }
        self.memory_before = Some((path.clone(), std::fs::read(&path).ok()));
        self.send(Msg::EditFile { target, path });
    }

    /// An instruction file was edited: an `AGENTS.md` reaches the
    /// orchestrator as system context; `SYSTEM.md` applies from the next
    /// conversation (as in-process).
    async fn memory_edited(&mut self, target: MemoryTarget, path: &std::path::Path) {
        let before = match self.memory_before.take() {
            Some((p, before)) if p == path => before,
            _ => None,
        };
        let now = std::fs::read(path).ok();
        if now == before {
            self.notice(
                NoticeLevel::Info,
                format!("{} is unchanged", path.display()),
            );
            return;
        }
        let next = format!(
            "saved {}; it applies from the next conversation (/new)",
            path.display()
        );
        let content = now
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let context = match target {
            MemoryTarget::System => None,
            MemoryTarget::Workspace | MemoryTarget::User => {
                crate::bootstrap::instructions::edited_context(path, &content)
            }
        };
        let Some(context) = context else {
            self.notice(NoticeLevel::Info, next);
            return;
        };
        let key = crate::runner::context::memory_key(&context);
        match self.push_context(&context, &key).await {
            Ok(()) => self.notice(
                NoticeLevel::Info,
                format!(
                    "saved {}: the agent reads the new content from its next model call; new conversations load it as usual",
                    path.display()
                ),
            ),
            Err(err) => self.notice(
                NoticeLevel::Warn,
                format!("{next} (the agent could not take it now: {err})"),
            ),
        }
    }

    /// Store the cost totals next to the conversation's session (as
    /// in-process), so a reattach shows them again.
    fn save_cost(&self, snapshot: &crate::ui::SessionCostSnapshot) {
        let Some(link) = &self.link else {
            return;
        };
        if let Err(err) = crate::runner::cost_store::save_cost(
            &self.config.scope.realm_root(),
            &link.session.to_string(),
            snapshot,
        ) {
            tracing::warn!(error = %err, "could not record the conversation's cost");
        }
    }

    /// Show `session`'s stored cost totals (none: zero).
    fn load_cost(&self, session: &SessionId) {
        let cost = crate::runner::cost_store::load_cost(
            &self.config.scope.realm_root(),
            &session.to_string(),
        )
        .unwrap_or_default();
        self.send(Msg::Status(StatusUpdate::CostTotals(cost)));
    }

    fn status_report(&self) {
        let mut lines = vec![format!(
            "rkat-tui {} (attach mode, experimental)",
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
        lines.push(format!(
            "workspace: {}",
            self.config.scope.context_root.display()
        ));
        lines.push(format!(
            "endpoint file: {}",
            self.config.endpoint_file.display()
        ));
        lines.push(
            "approval gate: in the daemon (requests show here; /rules and Shift+Tab act on the daemon)"
                .to_string(),
        );
        self.notice(NoticeLevel::Info, lines.join("\n"));
    }

    fn export(&self, typed: Option<&str>, text: &str) {
        use crate::paths::{FileMode, write_atomic};
        let workspace = &self.config.scope.context_root;
        let session = self
            .link
            .as_ref()
            .map(|link| link.session.to_string())
            .unwrap_or_default();
        let path = crate::runner::export::export_path(
            workspace,
            typed,
            &session,
            std::time::SystemTime::now(),
        );
        let trusted = if typed.is_some() {
            path.parent().map(std::path::Path::to_path_buf)
        } else {
            Some(workspace.clone())
        };
        match write_atomic(
            &path,
            text.as_bytes(),
            FileMode::Private,
            trusted.as_deref(),
        ) {
            Ok(()) => self.notice(
                NoticeLevel::Info,
                format!("exported the transcript to {}", path.display()),
            ),
            Err(err) => self.notice(NoticeLevel::Error, format!("export failed: {err}")),
        }
    }

    /// `/daemon stop`: SIGTERM to the pid the endpoint file names, once the
    /// file still names the daemon this client is attached to.
    async fn daemon_stop(&mut self) {
        let connected = self.link.as_ref().map(|link| link.endpoint.address);
        let liveness = launch::probe(&self.config.endpoint_file).await;
        match stop_target(&liveness, connected) {
            Ok(pid) => match terminate(pid) {
                Ok(()) => {
                    self.stopping = true;
                    self.notice(
                        NoticeLevel::Info,
                        format!("asked the daemon (pid {pid}) to shut down"),
                    );
                }
                Err(err) => {
                    self.notice(NoticeLevel::Error, format!("cannot stop the daemon: {err}"));
                }
            },
            Err(why) => self.notice(NoticeLevel::Warn, why),
        }
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
        let (events, last_seq) = replay_to_end(connection, &session, 0).await?;
        let messages = remote::transcript(connection, &session).await?;
        let identity = remote::identity(connection, &session).await.ok();
        let usage = remote::usage(connection, &session).await.ok();
        Ok::<_, ClientError>(Adopted {
            blocks: crate::runner::blocks_from_messages(&messages),
            session,
            stream,
            last_seq,
            identity,
            usage,
            context: last_context(&events),
        })
    })
    .await
    .map_err(|_| "timed out reading the new conversation".to_string())?
    .map_err(|err| err.to_string())
}

/// The endpoint the first connect attaches to: a live one, or (auto mode)
/// the one a started daemon writes.
async fn first_endpoint(
    file: &std::path::Path,
    spawn: Option<&SpawnPlan>,
    ui_tx: &mpsc::UnboundedSender<Msg>,
) -> Result<Endpoint, ClientError> {
    let mode = if spawn.is_some() {
        AttachMode::Auto
    } else {
        AttachMode::Existing
    };
    let decision = launch::decide(launch::probe(file).await, mode);
    match (decision, spawn) {
        (Decision::Attach(endpoint), _) => Ok(endpoint),
        (Decision::Start, Some(plan)) => {
            let _ = ui_tx.send(Msg::Status(StatusUpdate::Attach(Some(STARTING.into()))));
            let (pid, mut exited) = launch::spawn_daemon(
                &plan.program,
                &launch::with_endpoint_file(&plan.argv, file),
                &plan.log,
            )?;
            let _ = ui_tx.send(Msg::Notice {
                level: NoticeLevel::Info,
                text: format!(
                    "started rkat-tui-mob (pid {pid}; output in {})",
                    plan.log.display()
                ),
            });
            launch::wait_for_endpoint(file, launch::START_TIMEOUT, &mut exited, &plan.log).await
        }
        (Decision::Start | Decision::Refuse, _) => Err(ClientError::NoDaemon {
            file: file.to_path_buf(),
        }),
    }
}

/// Start the daemon again after it died (its endpoint file is stale) and
/// wait for its endpoint.
async fn restart(
    file: &std::path::Path,
    plan: &SpawnPlan,
    ui_tx: &mpsc::UnboundedSender<Msg>,
) -> Result<Endpoint, ClientError> {
    let _ = ui_tx.send(Msg::Status(StatusUpdate::Attach(Some(STARTING.into()))));
    let (pid, mut exited) = launch::spawn_daemon(
        &plan.program,
        &launch::with_endpoint_file(&plan.argv, file),
        &plan.log,
    )?;
    let _ = ui_tx.send(Msg::Notice {
        level: NoticeLevel::Warn,
        text: format!(
            "the daemon died (its endpoint file is stale); restarted rkat-tui-mob (pid {pid}; output in {})",
            plan.log.display()
        ),
    });
    launch::wait_for_endpoint(file, launch::START_TIMEOUT, &mut exited, &plan.log).await
}

/// Attach, find the orchestrator's session and open its stream: resumed
/// from the saved cursor when it names this daemon's stream of the session
/// (the events since come on the stream), else live after rendering the
/// conversation from its start.
async fn establish(endpoint: Endpoint, saved: Option<&Cursor>) -> Result<Established, ClientError> {
    let (connection, mut notifications) = Connection::attach(&endpoint, ATTACH_TIMEOUT).await?;
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
                // render; the mark drops what both deliver.
                let stream = remote::open_stream(&connection, None).await?;
                let (events, last_seq) = replay_to_end(&connection, &session, 0).await?;
                let messages = remote::transcript(&connection, &session).await?;
                (
                    stream,
                    Mark::rendered(&id, last_seq),
                    Some(crate::runner::blocks_from_messages(&messages)),
                    last_context(&events),
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
            endpoint_file: dir.join("mob.endpoint"),
            cursor: CursorFile::of_workspace(&scope.context_root),
            config: None,
            spawn: None,
            user_shell: Err("none".into()),
            endpoints: EndpointRegistry::from_endpoints(Vec::new()),
            lister: crate::models::live_lister(),
            ui: crate::approval::rules::UiSettings::default(),
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
            std::future::pending::<()>().await;
        });
        let (read, write) = tokio::io::split(client);
        let address: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
        let (connection, notifications) =
            Connection::attach_io(read, write, "t", address, ATTACH_TIMEOUT)
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
        });
        assert_eq!(stop_target(&live, Some(at)), Ok(42));
        assert!(stop_target(&live, Some("127.0.0.1:4001".parse().unwrap())).is_err());
        assert!(stop_target(&live, None).is_err());
        assert!(stop_target(&launch::Liveness::Absent, Some(at)).is_err());
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
