//! The client's loop between the UI and the daemon: [`Effect`]s become
//! the orchestrator's methods ([`super::remote`]), the member's events
//! become [`Msg`]s, and the connection is kept (re-attached with backoff,
//! with catch-up, when it drops).
//!
//! A turn, as the UI sees it, starts with [`Effect::StartTurn`] (or with a
//! run this UI did not start: another client's, or one the catch-up found,
//! announced with [`Msg::RemoteTurn`]) and ends once every run it saw
//! ended and the daemon reports no unfinished input of the conversation
//! (`rkat/active_inputs`): a steer that missed the boundary, whichever
//! client sent it, runs as a follow-up run of the same turn. The wait is
//! bounded by [`FOLLOW_UP_WAIT`]; without a daemon to ask the turn ends
//! [`RUN_GRACE`] after its last run. Its result is the runs' terminal
//! events: `run_completed` (the text and usage), `run_failed` (an
//! interruption when this client interrupted it or the class is
//! `cancelled`, else the failure).
//!
//! Every event is sent to the UI as [`Msg::Agent`] on the integrator
//! channel, so the turn messages stay in order with the events.
//!
//! What runs on this side of the agent (`!` shell mode, `/remember`,
//! `/memory`, `/export`, prices and the cost record, the model lists and
//! picker, unsent messages) is the [`Workbench`]'s; it reaches the
//! orchestrator through its [`AgentContext`] (`mob/append_system_context`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use meerkat_core::lifecycle::{InputId, RunId};
use meerkat_core::{AgentErrorClass, AgentEvent, EventEnvelope, Message, SessionId};
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
use crate::daemon::protocol::{self, AgentChoice, SetModeParams};
use crate::endpoints::{EndpointRegistry, ModelRefError, WireDialect, parse_model_ref};
use crate::models::{FetchResults, ModelCache};
use crate::ui::delivery::{PendingId, SteerFailure};
use crate::ui::{
    Block, Checklist, ChecklistView, CompactFailure, Effect, EscalationAnswer, EscalationId, Msg,
    NoticeLevel, RuleEffect, StatusUpdate, TurnFailure, TurnOutcome, TurnSummary,
};
use crate::ui::{Connection as LinkState, Departure, QuitDecision};
use crate::user_shell::UserShell;
use crate::workbench::context;
use crate::workbench::models::Current;
use crate::workbench::{
    AgentContext, ContextError, ContextSink, SavedUnsent, Workbench, WorkbenchParts,
};

mod commands;
mod depart;
mod link;
mod steers;
mod switch;
mod turn;

use link::next_notification;
use steers::Steers;

/// What is sent in place of a prompt that repeats the one a failed turn
/// already committed ([`turn::send_outbound`]).
const RETRY_INPUT: &str = "(retrying the request above)";

/// How many times a failed first connect is retried by itself (each
/// retry may start the daemon again): a daemon that fails the same way
/// every time (a bad config, a missing key) must not be restarted in a
/// loop. `/daemon start` retries once more.
const START_RETRIES: u32 = 3;
/// The wait before the first of those retries; each next one doubles it.
const START_RETRY_FIRST: Duration = Duration::from_millis(500);

/// How often the orchestrator's current session is read (another client's
/// `/new` or `/resume` moves it).
pub const SESSION_POLL: Duration = Duration::from_secs(1);

/// How often the mobs are listed while a tool that seats helpers runs.
pub const DISCOVER_EVERY: Duration = Duration::from_millis(250);

/// How long after such a tool starts the mobs are listed.
pub const DISCOVER_FOR: Duration = Duration::from_secs(10);

/// How many events of streams not known yet are held (see
/// [`Runner`]'s `orphans`).
const ORPHANS_MAX: usize = 2_000;

/// The notice shown on the first attach: what the daemon is.
pub const ATTACH_NOTICE: &str = "the agent runs in this workspace's rkat-tui-mob daemon, which keeps working when you /detach and stops on /quit when this rkat-tui started it (its approval requests show in every attached client; the first answer counts).";

/// How long a turn whose runs all ended waits for a follow-up run.
pub const RUN_GRACE: Duration = Duration::from_millis(300);

/// The longest a turn whose runs all ended stays open while the runtime
/// still holds unfinished input of the conversation (`rkat/active_inputs`:
/// a steer, this client's or another's, that runs as a follow-up run) or
/// a steer of this client is not written into a run yet. The follow-up
/// run's start delivers such a steer, so the turn does not end (and a new
/// one start with the steer's own text) before it. Past the bound the
/// turn ends with a notice; the runtime owns what it holds.
pub const FOLLOW_UP_WAIT: Duration = Duration::from_secs(10);

/// How soon a turn whose runs ended asks the daemon again while it still
/// holds unfinished input.
pub const INPUT_RECHECK: Duration = Duration::from_millis(100);

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
/// ([`depart::daemon_ended`]).
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
pub struct ClientConfig {
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
    /// Which agent the first attach works with (a reconnect asks for the
    /// agent it was bound to).
    pub agent: AgentChoice,
    /// The start token handed to a daemon this client starts, presented
    /// on every attach: the daemon counts the client that started it as
    /// its owner (`/quit` then stops a daemon nobody detached from).
    pub start_token: Option<String>,
    /// The user's `!` shell (this process's own: it runs as the user).
    pub user_shell: Arc<UserShell>,
    /// This process's LLM endpoints (the model picker, `/model`).
    pub endpoints: crate::endpoints::EndpointRegistry,
    /// Fetches the endpoints' model lists (tests list nothing).
    pub lister: crate::models::ModelLister,
    /// The `[ui]` settings of the `tui.toml` layers.
    pub ui: crate::config::UiSettings,
    /// The context-compaction policy of the `tui.toml` layers.
    pub compaction: crate::ui::CompactionPolicy,
    /// The cost readout's prices (the `[pricing]` overrides and the cached
    /// community list; refreshed in the background).
    pub pricing: crate::pricing::PricingSetup,
    /// How `/rehome` re-roots this client (`None`: it cannot, and says
    /// so).
    pub reroot: Option<Reroot>,
}

/// The configuration of this client for a command line
/// ([`Reroot::configure`]).
pub type Configure = Arc<
    dyn Fn(
            crate::cli::Args,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<ClientConfig, crate::TuiError>> + Send>,
        > + Send
        + Sync,
>;

/// How `/rehome` re-roots this client: the command line its configuration
/// came from, the configuration for that command line with another
/// workspace (production: the workspace's layers read again, its daemon
/// started on demand), and where a workspace's daemon writes its endpoint
/// file.
#[derive(Clone)]
pub struct Reroot {
    /// The command line.
    pub args: crate::cli::Args,
    /// The configuration for a command line.
    pub configure: Configure,
    /// Where a workspace's daemon writes its endpoint file.
    pub endpoint_files: super::rehome::EndpointFiles,
}

impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("workspace", &self.scope.context_root)
            .field("endpoint_files", &self.endpoint_files)
            .field("spawn", &self.spawn)
            .field("endpoints", &self.endpoints.names())
            .finish_non_exhaustive()
    }
}

impl ClientConfig {
    /// The `--attach` mode the config stands for.
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
    /// Model, provider and the configured endpoint its binding names.
    identity: Option<(String, String, Option<String>)>,
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
/// the link epoch it was started on ([`Runner::reporter`]).
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
    /// The conversation read again from its start ([`Runner::rerender`]).
    Rerendered(Result<Adopted, String>, Again),
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
    /// A prompt (`steer` `None`) of turn `turn`, or a steer, was sent, or
    /// why not ([`Outbound`]).
    Sent {
        turn: u64,
        steer: Option<PendingId>,
        result: Result<Option<InputId>, String>,
    },
    /// `rkat/recall` answered (or failed, in words).
    Recalled(Result<protocol::Recalled, String>),
    /// `rkat/active_inputs` answered question `check` of the turn (or
    /// failed, in words).
    ActiveInputs {
        check: u64,
        result: Result<usize, String>,
    },
    /// `/agents <name>`: the daemon's agents as read for it.
    AgentsRead {
        name: String,
        status: Option<protocol::Status>,
    },
    /// `/daemon stop` asked the daemon to stop (the notice), or did not.
    StopAsked(Result<String, String>),
    /// `/quit`'s decision, and whether the daemon is stopping.
    QuitDecided {
        decision: QuitDecision,
        stopping: bool,
    },
    /// A re-home's first half: the daemon stopped, or why not (the re-home
    /// did not start).
    RehomeStopped(Result<Box<RehomeStopped>, String>),
    /// A re-home's second half: how it ended.
    Rehomed(Box<Rehomed>),
}

/// The re-home's state between its halves ([`Report::RehomeStopped`]).
struct RehomeStopped {
    plan: super::rehome::Plan,
    endpoint: Endpoint,
    reroot: Reroot,
    agent: AgentChoice,
}

/// How a re-home ended ([`Report::Rehomed`]).
enum Rehomed {
    /// It moved; this client works on `config` from now on.
    Moved {
        config: ClientConfig,
        reroot: Reroot,
        agent: AgentChoice,
    },
    /// It moved, but this client cannot run there.
    Stranded,
    /// Nothing moved: the workspace's daemon starts again.
    Failed,
}

/// A prompt or a steer for the agent, sent by [`Runner::outbound`]'s task
/// in the order they were made.
struct Outbound {
    connection: Arc<Connection>,
    text: String,
    /// `Some`: a steer.
    steer: Option<PendingId>,
    /// The conversation of the turn that just failed with this prompt: it
    /// is sent as [`RETRY_INPUT`] when that prompt is still its last
    /// message.
    retry: Option<SessionId>,
    /// The turn it belongs to.
    turn: u64,
    reports: Reporter,
    ui_tx: mpsc::UnboundedSender<Msg>,
}

/// Why the current conversation is read again from its start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Again {
    /// The stream lost events the daemon no longer keeps.
    LostEvents,
    /// The daemon ended the conversation's stream: it seated the agent
    /// again (a model switch to a model it had no profile for).
    Reseated,
}

/// What a client shows of the conversation it switched to.
struct Adopted {
    session: SessionId,
    /// The orchestrator's stream, opened again.
    stream: String,
    blocks: Vec<Block>,
    /// The last durable sequence of the session (shown as the transcript).
    last_seq: u64,
    /// Model, provider and the configured endpoint its binding names.
    identity: Option<(String, String, Option<String>)>,
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
    /// Steers sent or admitted and not yet delivered, by the input the
    /// runtime admitted each as.
    steers: Steers,
    result: Option<Result<TurnSummary, TurnFailure>>,
    interrupted_at: Option<Instant>,
    force_cancelled: bool,
    /// Every run ended: the turn ends then unless another starts.
    finish_by: Option<Instant>,
    /// Every run ended and the runtime still held unfinished input: ask
    /// the daemon again then ([`INPUT_RECHECK`]).
    recheck_at: Option<Instant>,
    /// The `rkat/active_inputs` question this turn waits for (answers of
    /// an older one are dropped).
    settle: u64,
    /// The last answer named unfinished input.
    held_input: bool,
    /// An interrupt the daemon has not confirmed ends the turn then.
    give_up_at: Option<Instant>,
    /// The prompt that started it, as sent (`None`: another client's run).
    prompt: Option<String>,
    /// A steer of this client joined it.
    steered: bool,
    /// Which of the UI's turns it is (`Runner::turns`; 0: another
    /// client's run).
    id: u64,
}

impl Turn {
    fn record(&mut self, end: Result<TurnSummary, TurnFailure>) {
        self.result = Some(crate::ui::merge_turn_end(self.result.take(), end));
    }

    fn deadline(&self) -> Option<Instant> {
        [self.finish_by, self.give_up_at, self.recheck_at]
            .into_iter()
            .flatten()
            .min()
    }
}

/// Why a call to the daemon brought no answer.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CallFailure {
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

/// The client's runner: the loop between the UI and the daemon.
pub struct Runner {
    config: ClientConfig,
    /// The agent the daemon bound this client to (`None` before the first
    /// attach); a reconnect asks for it again.
    agent: Option<String>,
    /// An `/agents <name>` switch runs: the next link renders that agent's
    /// conversation from its start.
    agent_switch: bool,
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
    /// connection and is not applied ([`Runner::on_report`]).
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
    /// The dialect of the current model's provider.
    current_dialect: Option<WireDialect>,
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
    /// prompt is sent as the UI opens) or while a conversation switch
    /// runs: held until the link is up and no switch runs, or the first
    /// connect failed; `None` once released.
    held: Option<Vec<Effect>>,
    /// The first connect failed for good (held prompts then fail as not
    /// attached).
    first_failed: bool,
    /// When a failed connect is tried again by itself.
    retry_at: Option<Instant>,
    /// The automatic retries of the failed connect so far
    /// ([`START_RETRIES`]).
    retries: u32,
    /// The prompt of the turn that just failed, in its conversation.
    failed_prompt: Option<(SessionId, String)>,
    /// Numbers the UI's turns: a prompt's send failure ends the turn it was
    /// sent for, not a later one.
    turns: u64,
    /// The prompts and steers on their way, sent in order by one task
    /// ([`Self::outbound`]).
    outbound: Option<mpsc::UnboundedSender<Outbound>>,
    /// How the UI leaves the daemon, done once the loop ends
    /// ([`Self::depart`]).
    departure: Option<Departure>,
    /// A re-home runs: the daemon it stopped is no lost connection.
    rehoming: bool,
}

/// What a background task says to the UI ([`Runner::say`]).
#[derive(Clone)]
struct Say {
    ui_tx: mpsc::UnboundedSender<Msg>,
    farewells: Farewells,
}

impl Say {
    fn notice(&self, level: NoticeLevel, text: impl Into<String>) {
        let _ = self.ui_tx.send(Msg::Notice {
            level,
            text: text.into(),
        });
    }

    /// Say `text` now and again on the terminal after the UI closed.
    fn farewell(&self, level: NoticeLevel, text: String) {
        self.notice(level, text.clone());
        self.farewells.lock().push(text);
    }
}

/// Lines for the terminal after the UI closed ([`Runner::farewells`]).
pub type Farewells = Arc<parking_lot::Mutex<Vec<String>>>;

impl Runner {
    /// A runner on `config`; it connects once [`Runner::run`] runs.
    #[must_use]
    pub fn new(
        config: ClientConfig,
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
            agent: None,
            agent_switch: false,
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
            current_dialect: None,
            bench,
            admitted: Vec::new(),
            farewells: Farewells::default(),
            held: Some(Vec::new()),
            first_failed: false,
            retry_at: None,
            retries: 0,
            failed_prompt: None,
            turns: 0,
            outbound: None,
            departure: None,
            rehoming: false,
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
        self.say().farewell(level, text);
    }

    /// What a task says to the UI, and on the terminal after it closed.
    fn say(&self) -> Say {
        Say {
            ui_tx: self.ui_tx.clone(),
            farewells: Arc::clone(&self.farewells),
        }
    }

    /// Serve effects until the UI quits, then detach (the daemon and any
    /// running turn go on). Returns where the messages the daemon never
    /// got were saved, when there were any.
    pub async fn run(mut self) -> Option<SavedUnsent> {
        self.bench.start();
        // This client made the workspace's realm: it offers the realms
        // whose workspace is gone (the daemon it starts finds the realm
        // made and offers nothing).
        if let Some(offer) = self.config.scope.offer.take() {
            self.notice(NoticeLevel::Info, offer);
        }
        self.show(LinkState::Connecting);
        self.connect();
        loop {
            for effect in self.releasable() {
                self.handle_effect(effect).await;
            }
            tokio::select! {
                effect = self.effect_rx.recv() => match effect {
                    None | Some(Effect::Quit) => break,
                    // Held while a switch runs too: the prompt goes to the
                    // conversation the switch ends on, over its stream.
                    Some(effect @ Effect::StartTurn(_)) if self.held.is_some() || self.switching => {
                        self.held.get_or_insert_with(Vec::new).push(effect);
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
                () = sleep_until(self.retry_at) => self.retry(),
                () = sleep_until(self.session_check_at) => self.check_session(),
                joined = self.bench.background() => self.model_lists(joined),
            }
        }
        if let Some(departure) = self.departure.take() {
            self.depart(departure).await;
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

    fn show(&self, state: LinkState) {
        self.send(Msg::Status(StatusUpdate::Connection(state)));
    }

    /// A background task's `report`, started on the link `epoch`. What a
    /// task started on an earlier link learned about that connection (a
    /// switched-to stream, a mob stream, the session it read) is dropped:
    /// installed, a dead connection's stream id would silently mute the
    /// orchestrator; the reattach read everything again.
    fn on_report(&mut self, epoch: u64, report: Report) {
        if epoch != self.link_epoch {
            match report {
                Report::Switched { .. } | Report::Rerendered(..) | Report::MobOpened { .. } => {
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
            Report::Sent {
                turn,
                steer,
                result,
            } => self.sent(turn, steer, result),
            Report::Recalled(result) => self.recalled(result),
            Report::ActiveInputs { check, result } => self.active_inputs(check, result),
            Report::AgentsRead { name, status } => self.agents_read(name, status),
            Report::StopAsked(Ok(text)) => {
                self.stopping = true;
                self.notice(NoticeLevel::Info, text);
            }
            Report::StopAsked(Err(why)) => self.notice(NoticeLevel::Warn, why),
            Report::QuitDecided { decision, stopping } => {
                self.stopping |= stopping;
                self.send(Msg::QuitDecided(decision));
            }
            Report::RehomeStopped(stopped) => self.rehome_stopped(stopped),
            Report::Rehomed(rehomed) => self.rehomed(*rehomed),
            Report::ModelSet { switch, result } => match result {
                Ok(set) => {
                    if self
                        .link
                        .as_ref()
                        .is_some_and(|link| link.session == set.session_id)
                    {
                        let dialect = WireDialect::parse(&set.provider);
                        self.show_model(&switch.endpoint, dialect, &set.model);
                        // The agent was seated again: its stream is new.
                        if set.reseated {
                            self.rerender(set.session_id.clone(), Again::Reseated);
                        }
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
                    Ok(adopted) => self.adopted(adopted, own, warning, None),
                    Err(why) if own.is_some() => self.notice(
                        NoticeLevel::Error,
                        format!("could not switch conversations: {why}"),
                    ),
                    Err(why) => {
                        tracing::warn!(%why, "cannot show the orchestrator's new conversation");
                    }
                }
            }
            Report::Rerendered(result, again) => {
                self.switching = false;
                match result {
                    Ok(adopted) => self.adopted(adopted, None, None, Some(again)),
                    Err(why) => self.notice(
                        NoticeLevel::Warn,
                        format!("the conversation could not be read again: {why}"),
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

    /// Why this client has no connection now, as a notice says it.
    fn unattached(&self) -> String {
        let why = if self.stopping {
            "the daemon was stopped; /daemon start starts it again"
        } else if self.connecting.is_some() {
            "connecting to it"
        } else if self.first_failed {
            "the connect failed; /daemon start tries again"
        } else if self.retry_at.is_some() {
            "the connect is tried again shortly"
        } else {
            "reconnecting"
        };
        format!("not attached to the daemon ({why})")
    }

    /// The connection, or (none now) a warning that `what` did not happen
    /// and why ([`Self::unattached`]).
    fn link_or_say(&self, what: &str) -> Option<Arc<Connection>> {
        let connection = self.connection();
        if connection.is_none() {
            self.notice(NoticeLevel::Warn, format!("{}: {what}", self.unattached()));
        }
        connection
    }

    fn connection(&self) -> Option<Arc<Connection>> {
        self.link.as_ref().map(|link| Arc::clone(&link.connection))
    }

    /// Act on one effect of the UI. Nothing here waits on the daemon:
    /// every call is made by a task of its own, which reports back (a
    /// [`Report`]) or tells the UI itself, so the loop always takes the
    /// next effect, Esc included.
    async fn handle_effect(&mut self, effect: Effect) {
        match effect {
            Effect::StartTurn(text) => self.start_turn(&text),
            Effect::Steer { id, text } => self.steer(id, &text),
            Effect::Interrupt => self.interrupt(),
            Effect::RecallQueued => self.recall(),
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
            Effect::DaemonStop => self.daemon_stop(),
            Effect::Rehome(reference) => self.rehome(reference),
            Effect::DaemonStart => self.daemon_start(),
            Effect::Agents(None) => self.list_agents(),
            Effect::Audit(lines) => self.show_audit(lines),
            Effect::Schedules(action) => self.schedules(action),
            Effect::Agents(Some(name)) => self.switch_agent(name),
            Effect::DecideQuit => self.decide_quit(),
            // Done once the loop ends: [`Effect::Quit`] follows.
            Effect::Leave(departure) => self.departure = Some(departure),
            Effect::NewSession => self.switch_conversation(None),
            Effect::ResumeSession(reference) => self.resume_session(reference),
            Effect::RequestResumePicker => self.show_resume_picker(),
            Effect::RequestRewindMenu => {
                self.notice(NoticeLevel::Warn, sessions::REWIND_UNAVAILABLE);
            }
            Effect::RequestModelPicker { refresh } => {
                let current = current(self.current_endpoint.as_deref(), self.model.as_ref());
                self.bench.request_model_picker(refresh, current);
            }
            Effect::Remember(text) => self.bench.remember(&text, &self.linked()),
            Effect::OpenMemory(target) => self.bench.open_memory(target),
            Effect::MemoryEdited { target, path } => {
                let linked = self.linked();
                self.bench.memory_edited(target, &path, &linked);
            }
            Effect::CostSnapshot(snapshot) => {
                if let Some(link) = &self.link {
                    self.bench.save_cost(&link.session.to_string(), &snapshot);
                }
            }
            Effect::RespondEscalation(id, answer) => self.respond_escalation(id, &answer),
            Effect::SetMode(mode) => self.set_mode(mode.name()),
            Effect::ResumeAutoMode => {
                self.set_mode(crate::approval::mode::PermissionMode::Auto.name());
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
                self.outcome_command::<protocol::method::Rules>(command, "/permissions");
            }
            Effect::RemoveRule(n) => {
                self.outcome_command::<protocol::method::Rules>(
                    RuleCommand::Remove { n },
                    "/permissions",
                );
            }
            Effect::ListRules => {
                self.outcome_command::<protocol::method::Rules>(RuleCommand::List, "/permissions");
            }
            Effect::TrustProject => {
                self.outcome_command::<protocol::method::Trust>(protocol::Empty {}, "/trust");
            }
            Effect::SaveUnsent(held) => self.bench.hold_unsent(held),
            Effect::SetChecklistView(view) => self.set_checklist_view(view),
            Effect::Quit => {}
        }
    }

    /// The orchestrator as the workbench reaches it, over this connection.
    fn linked(&self) -> AgentContext {
        AgentContext::new(
            self.connection()
                .map(|connection| Arc::new(Linked(connection)) as Arc<dyn ContextSink>),
        )
    }
}

/// The agent over this client's connection, as the workbench reaches it:
/// `mob/append_system_context`, bounded.
struct Linked(Arc<Connection>);

impl ContextSink for Linked {
    fn append(
        &self,
        text: String,
        key: String,
    ) -> futures::future::BoxFuture<'static, Result<(), ContextError>> {
        let connection = Arc::clone(&self.0);
        Box::pin(async move {
            bounded(
                CALL_TIMEOUT,
                remote::append_system_context(&connection, &text, &key),
            )
            .await
            .map_err(|err| ContextError::NotTaken(err.to_string()))
        })
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
    use super::commands::{agent_lines, others_notice, report_lines, schedules_text};
    use super::link::{Probe, classify};
    use super::turn::model_switch;
    use super::*;

    /// The `/quit` dialog says what terminating does to the agent's
    /// schedules; `/schedules` prices each schedule's runs.
    #[test]
    fn listing_names_the_schedules() {
        let row = protocol::ScheduleRow {
            id: "id".into(),
            name: "nightly".into(),
            expression: "every 86400s from 2026-11-11 02:00:00Z".into(),
            next: Some("2026-11-12 02:00:00Z".into()),
            state: "active".into(),
            fired: 1,
            cost: None,
        };
        assert_eq!(
            schedules_text(&[row], &crate::pricing::PriceTable::default()),
            "1 schedule of this agent:\n  nightly  every 86400s from 2026-11-11 02:00:00Z  next 2026-11-12 02:00:00Z  active  fired 1x  $0"
        );
    }

    /// `/status`'s daemon side: the realm, confinement, mode, trust and
    /// helper lines, in their order.
    #[test]
    fn the_start_up_notice_names_the_other_agents() {
        let row = |name: &str, running| protocol::AgentRow {
            name: name.into(),
            session_id: None,
            running,
            clients: 0,
            detached: false,
            schedules: 0,
        };
        assert_eq!(
            others_notice(&[row("main-mine", Some(false))], "main-mine"),
            None
        );
        let text = others_notice(
            &[
                row("main-abcd", Some(true)),
                row("main-mine", Some(false)),
                row("main-efgh", Some(false)),
                row("main-ijkl", None),
            ],
            "main-mine",
        )
        .unwrap();
        assert!(
            text.starts_with(
                "other agents work in this workspace: main-abcd is running, main-efgh is idle, main-ijkl is unknown;"
            ),
            "{text}"
        );
        assert!(text.contains("-c joins the most recent"), "{text}");
    }

    #[test]
    fn agents_are_listed_with_state_conversation_and_clients() {
        let session = SessionId::new();
        let rows = vec![
            protocol::AgentRow {
                name: "main-aaaa".into(),
                session_id: Some(session.clone()),
                running: Some(true),
                clients: 1,
                detached: false,
                schedules: 0,
            },
            protocol::AgentRow {
                name: "main-bbbb".into(),
                session_id: None,
                running: Some(false),
                clients: 0,
                detached: true,
                schedules: 0,
            },
            protocol::AgentRow {
                name: "main-cccc".into(),
                session_id: None,
                running: None,
                clients: 1,
                detached: false,
                schedules: 0,
            },
        ];
        let lines = agent_lines(&rows, Some("main-aaaa"));
        let short = &session.to_string()[..8];
        assert_eq!(lines[0], "3 agents in this workspace:");
        assert_eq!(
            lines[1],
            format!("  main-aaaa · running · conversation {short} · 1 client (this rkat-tui)")
        );
        assert_eq!(
            lines[2],
            "  main-bbbb · idle · no conversation · 0 clients (detached)"
        );
        // Unknown is not idle.
        assert_eq!(
            lines[3],
            "  main-cccc · unknown · no conversation · 1 client"
        );
        assert!(lines[4].contains("/agents <name>"));
    }

    #[test]
    fn the_report_names_the_realm_confinement_mode_trust_and_helpers() {
        use crate::approval::desk::ModeStatus;
        use crate::approval::mode::PermissionMode;
        use std::path::PathBuf;
        let report = protocol::Report {
            realm: PathBuf::from("/w/.rkat/tui"),
            realm_id: "tui".into(),
            confinement: "workspace".into(),
            mode: ModeStatus {
                mode: PermissionMode::Auto,
                paused: true,
                denials: (1, 3, 2, 20),
            },
            realm_rules: PathBuf::from("/w/.rkat/tui/tui.toml"),
            project_inputs: vec![protocol::TrustedFile {
                path: PathBuf::from("/w/.rkat/mcp.toml"),
                trust: "not trusted: it changed".into(),
            }],
            helper_depth: 1,
            watches: Vec::new(),
        };
        assert_eq!(
            report_lines(&report),
            [
                "realm: /w/.rkat/tui (id tui)",
                "shell confinement: workspace",
                "permission mode: auto, PAUSED (consecutive denials 1/3, total 2/20)",
                "realm rules: /w/.rkat/tui/tui.toml",
                "project /w/.rkat/mcp.toml: not trusted: it changed",
                "helpers: depth limit on (max_depth 1: the agent's helpers start none of their own)",
            ]
        );
        let off = protocol::Report {
            helper_depth: 0,
            mode: ModeStatus {
                mode: PermissionMode::Plan,
                paused: false,
                denials: (0, 3, 0, 20),
            },
            ..report
        };
        let lines = report_lines(&off);
        assert_eq!(
            lines[2],
            format!("permission mode: {}", PermissionMode::Plan.label())
        );
        assert_eq!(
            lines.last().unwrap(),
            "helpers: depth limit 0, every helper is refused"
        );
        let watching = protocol::Report {
            watches: vec!["watch 1: src/** (once)".into()],
            ..off
        };
        assert_eq!(
            report_lines(&watching).last().unwrap(),
            "file watches: watch 1: src/** (once)"
        );
    }

    /// A runner on a workspace in `dir`, never connected.
    fn runner(dir: &std::path::Path) -> (Runner, mpsc::UnboundedReceiver<Msg>) {
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
        let config = ClientConfig {
            endpoint_files: vec![dir.join("mob.endpoint")],
            cursor: CursorFile::of_realm(&scope.realm_root()),
            config: None,
            spawn: None,
            address: None,
            remote: None,
            start: super::super::StartAt::Current,
            agent: AgentChoice::Recent,
            start_token: None,
            user_shell: Arc::new(UserShell::of_user(dir, Duration::from_secs(1))),
            endpoints: EndpointRegistry::from_endpoints(Vec::new()),
            lister: crate::models::live_lister(),
            ui: crate::config::UiSettings::default(),
            compaction: crate::ui::CompactionPolicy::default(),
            pricing: crate::pricing::PricingSetup::default(),
            reroot: None,
            scope,
        };
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let (_effects, effect_rx) = mpsc::unbounded_channel();
        (Runner::new(config, ui_tx, effect_rx), ui_rx)
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
            let timeout = crate::daemon::handshake::TIMEOUT;
            let pending =
                crate::daemon::handshake::read_attach(&mut lines, &mut server_write, "t", timeout)
                    .await
                    .unwrap();
            crate::daemon::handshake::acknowledge(&mut server_write, &pending, None, None, timeout)
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
            "t",
            protocol::AttachParams::default(),
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

    /// The finding this guards: the loop awaited the daemon's answer to a
    /// prompt (and to `/status`, `/agents`, …) inline, so an Esc behind a
    /// slow call waited up to its 15 s bound. Against a daemon that
    /// answers nothing after the attach, every effect returns at once and
    /// Esc interrupts the turn the unanswered prompt started.
    #[tokio::test]
    async fn no_effect_waits_on_a_daemon_that_does_not_answer() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut ui_rx) = runner(dir.path());
        runner.link = Some(link("live", SessionId::new()).await);
        let quick = Duration::from_secs(2);
        for effect in [
            Effect::StartTurn("hello".into()),
            Effect::Steer {
                id: PendingId(1),
                text: "and this".into(),
            },
            Effect::StatusReport,
            Effect::Agents(None),
            Effect::Audit(None),
            Effect::Schedules(None),
            Effect::ListRules,
            Effect::SetMode(crate::approval::mode::PermissionMode::Plan),
            Effect::DecideQuit,
            Effect::Interrupt,
        ] {
            let name = format!("{effect:?}");
            let done = tokio::time::timeout(quick, runner.handle_effect(effect)).await;
            assert!(done.is_ok(), "{name} waited on the daemon");
        }
        assert!(runner.turn.active, "the unanswered prompt's turn runs");
        assert!(
            runner.turn.interrupted_at.is_some(),
            "Esc interrupted it at once"
        );
        // The steer is expected at the next boundary while its send runs.
        assert!(runner.turn.steers.holds(PendingId(1)));
        // Nothing was answered: no turn end, no acknowledgement yet.
        while let Ok(msg) = ui_rx.try_recv() {
            assert!(
                !matches!(msg, Msg::TurnFinished(_) | Msg::SteerAck { .. }),
                "{msg:?}"
            );
        }
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

    /// A steer's answer: the input it names is matched against the
    /// boundary events (one applied before the answer delivers on it); an
    /// answer that names none is acknowledged and shown delivered with a
    /// notice, never matched to another input.
    #[tokio::test]
    async fn a_steers_answer_files_it_under_its_input() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut ui_rx) = runner(dir.path());
        runner.turn = Turn {
            active: true,
            id: 1,
            ..Turn::default()
        };
        let early = InputId::new();
        for id in 1..=3 {
            runner.turn.steers.sending(PendingId(id));
        }
        // 2's input joined the run before its send answered.
        assert_eq!(runner.turn.steers.applied(&early), None);
        runner.sent(1, Some(PendingId(1)), Ok(Some(InputId::new())));
        runner.sent(1, Some(PendingId(2)), Ok(Some(early)));
        runner.sent(1, Some(PendingId(3)), Ok(None));
        let mut seen = Vec::new();
        while let Ok(msg) = ui_rx.try_recv() {
            seen.push(msg);
        }
        let delivered: Vec<PendingId> = seen
            .iter()
            .filter_map(|m| match m {
                Msg::SteerDelivered(id) => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(delivered, [PendingId(2), PendingId(3)], "{seen:?}");
        assert!(
            seen.iter()
                .any(|m| matches!(m, Msg::Notice { text, .. } if text == turn::UNNAMED_INPUT))
        );
        assert_eq!(
            seen.iter()
                .filter(|m| matches!(m, Msg::SteerAck { result: Ok(()), .. }))
                .count(),
            3
        );
        // 1 waits for its own input; 3 is not followed any more.
        assert!(runner.turn.steers.holds(PendingId(1)));
        assert!(!runner.turn.steers.holds(PendingId(3)));
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

    /// The probe is async: a listener accepts, a closed port refuses.
    #[tokio::test]
    async fn the_address_probe_tells_a_listener_from_a_closed_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap();
        assert_eq!(super::link::probe_address(open).await, Probe::Accepts);
        drop(listener);
        assert_eq!(super::link::probe_address(open).await, Probe::Refused);
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

    /// A steer of this client the runtime still holds when the turn's
    /// runs completed runs as a follow-up run: the turn waits for that run
    /// (however late it starts) and marks the steer delivered at its
    /// start, never ending first. Without a held steer the short grace
    /// applies.
    #[tokio::test]
    async fn a_held_steer_keeps_the_turn_open_for_its_follow_up_run() {
        use crate::ui::delivery::PendingId;
        use meerkat_core::types::{ContentInput, RunInput, TranscriptMessageIdentity};
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut rx) = runner(dir.path());
        let session = SessionId::new();
        let envelope = |seq, payload| EventEnvelope::new("test", seq, None, payload);
        let started = |run: &RunId| AgentEvent::RunStarted {
            session_id: session.clone(),
            identity: TranscriptMessageIdentity {
                run_id: Some(run.clone()),
                ..TranscriptMessageIdentity::default()
            },
            input: RunInput::Content {
                content: ContentInput::Text("x".into()),
            },
            request_reasoning: None,
        };
        let completed = |run: &RunId| AgentEvent::RunCompleted {
            session_id: session.clone(),
            identity: TranscriptMessageIdentity {
                run_id: Some(run.clone()),
                ..TranscriptMessageIdentity::default()
            },
            result: "done".into(),
            structured_output: None,
            extraction_required: false,
            usage: meerkat_core::types::CumulativeUsage::default(),
            terminal_cause_kind: None,
            assistant_message_id: None,
        };
        runner.turn = Turn {
            active: true,
            ..Turn::default()
        };
        let (first, follow_up) = (RunId::new(), RunId::new());
        runner.on_event(envelope(1, started(&first)), None);
        runner.turn.steers.sending(PendingId(2));
        runner.turn.steers.admitted(PendingId(2), InputId::new());
        runner.on_event(envelope(2, completed(&first)), None);
        let wait = runner.turn.finish_by.unwrap() - Instant::now();
        assert!(wait > RUN_GRACE * 2, "{wait:?}");
        // Past the run grace the turn is still open.
        runner.turn.finish_by = runner.turn.finish_by.map(|at| at.max(Instant::now()));
        runner.on_deadline();
        assert!(runner.turn.active);
        while rx.try_recv().is_ok() {}
        runner.on_event(envelope(3, started(&follow_up)), None);
        assert!(matches!(
            rx.try_recv(),
            Ok(Msg::SteerDelivered(PendingId(2)))
        ));
        assert!(matches!(rx.try_recv(),
            Ok(Msg::Agent(env)) if matches!(env.payload, AgentEvent::RunStarted { .. })));
        assert!(rx.try_recv().is_err(), "the turn did not end");
        runner.on_event(envelope(4, completed(&follow_up)), None);
        let wait = runner.turn.finish_by.unwrap() - Instant::now();
        assert!(wait <= RUN_GRACE, "nothing held: the short grace {wait:?}");
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
