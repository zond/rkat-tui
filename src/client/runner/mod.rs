//! The client's loop between the UI and the daemon: [`Effect`]s become
//! the agent's methods ([`super::remote`]), the member's events
//! become [`Msg`]s, and the connection is kept (re-attached with backoff,
//! with catch-up, when it drops).
//!
//! A turn, as the UI sees it, starts with [`Effect::StartTurn`] (or with a
//! run this UI did not start: another client's, or one the catch-up found,
//! announced with [`Msg::RemoteTurn`]) and ends when the daemon says the
//! agent is idle after a run the turn saw end ([`protocol::IDLE`],
//! decided in [`crate::daemon::activity`]): a steer that missed the
//! boundary, whichever client sent it, runs as a follow-up run of the
//! same turn first. A turn ends without that only when the daemon stops,
//! or an interrupt it never confirms passes [`INTERRUPT_DEADLINE`]; a
//! lost connection leaves it running, and the reattach (the daemon tells
//! a client whether its agent is idle as it attaches) settles it. Its
//! result is the runs' terminal
//! events: `run_completed` (the text and usage), `run_failed` (an
//! interruption when this client interrupted it or the class is
//! `cancelled`, else the failure).
//!
//! Every event is sent to the UI as [`Msg::Agent`] on the client runner
//! channel, so the turn messages stay in order with the events.
//!
//! What runs on this side of the agent (`!` shell mode, `/remember`,
//! `/memory`, `/export`, prices and the cost record, the model lists and
//! picker, unsent messages) is the [`Workbench`]'s; it reaches the
//! agent through its [`AgentContext`] (`mob/append_system_context`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use meerkat_core::lifecycle::{InputId, RunId};
use meerkat_core::{AgentErrorClass, AgentEvent, EventEnvelope, Message, SessionId};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::ClientError;
use super::approvals::{Approvals, decision_params};
use super::catchup::{Cursor, CursorFile, Mark};
use super::helpers::{Action as FollowerAction, Followers, HelperId};
use super::launch::{self, Decision};
use super::push::Push;
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
    NoticeLevel, RuleEffect, StatusUpdate, TurnFailure, TurnSummary,
};
use crate::ui::{Connection as LinkState, Departure, QuitDecision};
use crate::user_shell::UserShell;
use crate::workbench::context;
use crate::workbench::models::Current;
use crate::workbench::{
    AgentContext, ContextError, ContextSink, SavedUnsent, Workbench, WorkbenchParts,
};

mod commands;
mod conn;
mod depart;
mod link;
mod steers;
mod switch;
mod turn;

use conn::{Conn, ConnEvent, Leaving};
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

/// How many events of streams not known yet are held (see
/// [`Runner`]'s `orphans`).
const ORPHANS_MAX: usize = 2_000;

/// The notice shown on the first attach: what the daemon is.
pub const ATTACH_NOTICE: &str = "the agent runs in this workspace's rkat-tui-mob daemon, which keeps working when you /detach and stops on /quit when this rkat-tui started it (its approval requests show in every attached client; the first answer counts).";

/// How long after the first interrupt a second one force-cancels.
pub const STOP_GRACE: Duration = Duration::from_secs(1);

/// How long an interrupted turn waits for the daemon to report its end
/// before the UI is given it back.
pub const INTERRUPT_DEADLINE: Duration = Duration::from_secs(10);

/// Bound on the calls that answer at once (sends, stops).
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on a conversation switch (the daemon retires and seats the
/// agent) and on reading the new conversation.
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

/// The model a session runs on, as the engine names it, and the
/// configured endpoint its binding names (none: a built-in provider).
#[derive(Debug, Clone)]
pub(super) struct SessionModel {
    /// The model id.
    model: String,
    /// The engine's provider.
    provider: String,
    /// The configured endpoint.
    endpoint: Option<String>,
}

impl SessionModel {
    /// The endpoint the status bar names: the configured one, else the
    /// provider (a built-in endpoint is named after it).
    fn endpoint_name(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(&self.provider)
    }
}

/// The model the status bar shows, on the endpoint it names.
#[derive(Debug, Clone)]
pub(super) struct ShownModel {
    /// The endpoint's name.
    endpoint: String,
    /// The model id.
    model: String,
}

/// One attached connection with the agent's stream open.
pub(super) struct Link {
    connection: Arc<Connection>,
    notifications: mpsc::UnboundedReceiver<Notification>,
    stream: String,
    session: SessionId,
    endpoint: Endpoint,
}

/// A link and what the catch-up read on it.
pub(super) struct Established {
    link: Link,
    /// Where the stream resumes (or the render from the start ended).
    mark: Mark,
    /// The conversation rendered from its start, when the stream could not
    /// resume from the saved cursor.
    reset: Option<Vec<Block>>,
    /// The model the session runs on.
    identity: Option<SessionModel>,
    /// The session's total usage, its totals by model and the context of
    /// its last model call (the gauge's seed), read after the replay.
    usage: Option<protocol::Usage>,
    /// The live notifications that arrived before the usage answer: their
    /// turns are in `usage` already, so they are shown before the counter
    /// is set to it.
    queued: Vec<Notification>,
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
    /// The daemon answered `/compact`'s request ([`Runner::compact_asked`]).
    CompactAsked(Result<(), String>),
    /// The mobs the daemon hosts (listed once per attach and per
    /// conversation; later ones are pushed, [`protocol::MOB`]).
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
    /// A conversation read from its start (`why` says why) and what it
    /// shows.
    Switched {
        result: Result<Adopted, String>,
        why: Shown,
        warning: Option<String>,
    },
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
    /// `rkat/inputs` answered for the steers this client still followed
    /// (or failed, in words).
    InputFates(Result<protocol::InputFates, String>),
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

/// Why a conversation is shown from its start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    /// This client's `/new`.
    New,
    /// This client's `/resume`.
    Resumed,
    /// Another client switched the agent's conversation.
    Others,
    /// The stream lost events the daemon no longer keeps (the same
    /// conversation).
    LostEvents,
    /// The daemon ended the conversation's stream: it seated the agent
    /// again (a model switch to a model it had no profile for).
    Reseated,
}

impl Shown {
    /// This client asked for it.
    fn own(self) -> bool {
        matches!(self, Self::New | Self::Resumed)
    }
}

/// What a client shows of the conversation it switched to.
struct Adopted {
    session: SessionId,
    /// The agent's stream, opened again.
    stream: String,
    blocks: Vec<Block>,
    /// The last durable sequence of the session (shown as the transcript).
    last_seq: u64,
    /// The model the session runs on.
    identity: Option<SessionModel>,
    /// The session's total usage, its totals by model and the context of
    /// its last model call (the gauge).
    usage: Option<protocol::Usage>,
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
    /// The runs that ended in it: the daemon's idle notice names the run
    /// it follows ([`protocol::IdleNote::last_run`]).
    ended: HashSet<RunId>,
    result: Option<Result<TurnSummary, TurnFailure>>,
    interrupted_at: Option<Instant>,
    force_cancelled: bool,
    /// The unconfirmed interrupt was followed by a hard cancel of the
    /// agent's current run ([`Runner::on_deadline`]).
    escalated: bool,
    /// The run `/compact` started on an idle agent ([`Runner::compact_asked`]):
    /// only its compaction events reach the UI.
    quiet: bool,
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
        self.give_up_at
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
    /// The agent picker's "new agent" row: connects ask for a new agent
    /// until one is bound.
    seat_new: bool,
    ui_tx: mpsc::UnboundedSender<Msg>,
    effect_rx: mpsc::UnboundedReceiver<Effect>,
    /// The connection's lifecycle.
    conn: Conn,
    reports_tx: mpsc::UnboundedSender<(u64, Report)>,
    reports_rx: mpsc::UnboundedReceiver<(u64, Report)>,
    /// Counts the links made: a report started on an earlier link (a
    /// connection lost since) names a stream or a session of that
    /// connection and is not applied ([`Runner::on_report`]).
    link_epoch: u64,
    mark: Option<Mark>,
    turn: Turn,
    /// This client's steers on their way to the agent, followed across
    /// turns by the inputs the runtime admitted them as.
    steers: Steers,
    /// What the daemon said last of this agent's being idle.
    idle: Option<protocol::IdleNote>,
    /// What became of the followed steers is being asked (`rkat/inputs`);
    /// `Some(true)`: an idle notice came meanwhile, ask again on the answer.
    asking_fates: Option<bool>,
    /// The model and endpoint as last read or switched to.
    model: Option<ShownModel>,
    /// The daemon the events shown came from (the cursor names it, see
    /// [`super::catchup::daemon_id`]: a restarted daemon numbers its events
    /// in a new space).
    daemon_id_seen: Option<String>,
    /// The approval dialogs shown.
    approvals: Approvals,
    /// The agent's helpers.
    followers: Followers,
    /// Events of streams opened but not registered yet, by stream id.
    orphans: Orphans<meerkat_mob::AttributedEvent>,
    /// The mobs the daemon said host helpers (listed at the attach,
    /// pushed since): one is opened once it is this agent's
    /// ([`Followers::should_open`], which a helper's session or a council
    /// can make true later).
    known_mobs: HashSet<String>,
    /// The conversation the daemon last said the agent moved to
    /// ([`protocol::SWITCHED`]) while a switch of this client's ran: shown
    /// once that switch ends, unless it ended there.
    pushed_session: Option<SessionId>,
    /// A conversation switch (this client's, or adopting another's) runs.
    switching: bool,
    /// The last `/model` switch (carried to a `/new` conversation).
    last_switch: Option<ModelSwitch>,
    /// The dialect of the current model's provider.
    current_dialect: Option<WireDialect>,
    /// `!` shell mode, `/remember`, `/memory`, `/export`, prices, the
    /// model lists and unsent messages.
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
            seat_new: false,
            ui_tx,
            effect_rx,
            conn: Conn::Idle,
            reports_tx,
            reports_rx,
            link_epoch: 0,
            mark: None,
            turn: Turn::default(),
            steers: Steers::default(),
            idle: None,
            asking_fates: None,
            model: None,
            daemon_id_seen: None,
            approvals: Approvals::default(),
            followers: Followers::default(),
            orphans: Orphans::default(),
            known_mobs: HashSet::new(),
            pushed_session: None,
            switching: false,
            last_switch: None,
            current_dialect: None,
            bench,
            admitted: Vec::new(),
            farewells: Farewells::default(),
            held: Some(Vec::new()),
            failed_prompt: None,
            turns: 0,
            outbound: None,
            departure: None,
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
        self.connect(true, 0);
        loop {
            for effect in self.releasable() {
                self.handle_effect(effect);
            }
            tokio::select! {
                effect = self.effect_rx.recv() => match effect {
                    None | Some(Effect::Quit) => break,
                    // Held while a switch runs too: the prompt goes to the
                    // conversation the switch ends on, over its stream.
                    Some(effect @ Effect::StartTurn(_)) if self.held.is_some() || self.switching => {
                        self.held.get_or_insert_with(Vec::new).push(effect);
                    }
                    Some(effect) => self.handle_effect(effect),
                },
                event = self.conn.next() => match event {
                    ConnEvent::Note(note) => self.on_notification(note),
                    ConnEvent::Lost => self.lost(),
                    ConnEvent::Connected(joined) => self.connected(joined),
                    ConnEvent::RetryDue => self.retry(),
                },
                Some((epoch, report)) = self.reports_rx.recv() => self.on_report(epoch, report),
                () = sleep_until(self.turn.deadline()) => self.on_deadline(),
                joined = self.bench.background() => self.model_lists(joined),
            }
        }
        if let Some(departure) = self.departure.take() {
            self.depart(departure).await;
        }
        self.save_cursor();
        self.bench.cancel_shell();
        if let Conn::Connecting { task, .. } = &self.conn {
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
        let ready =
            matches!(self.conn, Conn::Failed) || (self.conn.link().is_some() && !self.switching);
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
    /// agent; the reattach read everything again.
    fn on_report(&mut self, epoch: u64, report: Report) {
        if epoch != self.link_epoch {
            match report {
                Report::Switched { .. } | Report::MobOpened { .. } => {
                    tracing::debug!("dropped a report of a connection lost since");
                    return;
                }
                // The loss ended that switch; one started since is not.
                Report::SwitchRefused(level, text) => {
                    self.notice(level, text);
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
            Report::InputFates(result) => self.input_fates(result),
            Report::AgentsRead { name, status } => self.agents_read(name, status),
            Report::StopAsked(Ok(text)) => {
                self.stopped();
                self.notice(NoticeLevel::Info, text);
            }
            Report::StopAsked(Err(why)) => self.notice(NoticeLevel::Warn, why),
            Report::QuitDecided { decision, stopping } => {
                if stopping {
                    self.stopped();
                }
                self.send(Msg::QuitDecided(decision));
            }
            Report::RehomeStopped(stopped) => self.rehome_stopped(stopped),
            Report::Rehomed(rehomed) => self.rehomed(*rehomed),
            Report::ModelSet { switch, result } => match result {
                Ok(set) => {
                    if self
                        .conn
                        .link()
                        .is_some_and(|link| link.session == set.session_id)
                    {
                        let dialect = WireDialect::parse(&set.provider);
                        self.show_model(&switch.endpoint, dialect, &set.model);
                        // The agent was seated again: its stream is new.
                        if set.reseated {
                            self.rerender(set.session_id.clone(), Shown::Reseated);
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
            Report::CompactAsked(asked) => self.compact_asked(asked),
            Report::Interrupt(err) => {
                self.notice(NoticeLevel::Error, format!("interrupt incomplete: {err}"));
            }
            Report::Mobs(mobs) => {
                self.known_mobs.extend(mobs);
                self.open_known_mobs();
            }
            Report::MobOpened { mob, opened } => match opened {
                Ok((stream, members)) => {
                    if self.conn.link().is_none() {
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
                    // A helper's session may make a mob this agent's.
                    self.open_known_mobs();
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
            Report::Switched {
                result,
                why: shown,
                warning,
            } => {
                self.switching = false;
                match result {
                    Ok(adopted) => {
                        self.adopted(adopted, shown, warning);
                        self.follow_pushed_session();
                    }
                    Err(why) if shown.own() => {
                        self.notice(
                            NoticeLevel::Error,
                            format!("could not switch conversations: {why}"),
                        );
                        self.follow_pushed_session();
                    }
                    Err(why) if shown == Shown::Others => {
                        tracing::warn!(%why, "cannot show the agent's new conversation");
                    }
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
                self.follow_pushed_session();
            }
            Report::Tasks { session, tasks } => {
                // A list read for a conversation left since is not shown.
                if self.conn.link().is_none_or(|link| link.session != session) {
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
        format!("not attached to the daemon ({})", self.conn.unattached())
    }

    /// The daemon stopped, or confirmed it stops: attached, the coming
    /// loss of the link is not a lost connection; otherwise a connect
    /// under way is given up.
    fn stopped(&mut self) {
        match &mut self.conn {
            Conn::Up { leaving, .. } => *leaving = Some(Leaving::Stop),
            Conn::Connecting { task, .. } => {
                task.abort();
                self.conn = Conn::Stopped;
            }
            _ => self.conn = Conn::Stopped,
        }
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
        self.conn.link().map(|link| Arc::clone(&link.connection))
    }

    /// Act on one effect of the UI. Nothing here waits on the daemon (it
    /// is not `async`): every call is made by a task of its own, which
    /// reports back (a [`Report`]) or tells the UI itself, so the loop
    /// always takes the next effect, Esc included.
    fn handle_effect(&mut self, effect: Effect) {
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
                let session = self.conn.link()
                    .map(|link| link.session.to_string())
                    .unwrap_or_default();
                self.bench.export(path.as_deref(), &text, &session);
            }
            Effect::DaemonStop => self.daemon_stop(),
            Effect::Rehome(reference) => self.rehome(reference),
            Effect::DaemonStart => self.daemon_start(),
            Effect::Agents(None) => self.show_agent_picker(),
            Effect::NewAgent => self.new_agent(),
            Effect::RetireAgent(name) => self.retire_agent(name),
            Effect::Audit(lines) => self.show_audit(lines),
            Effect::Schedules(action) => self.schedules(action),
            Effect::PickedSchedule(op, id) => self.picked_schedule(op, id),
            Effect::Agents(Some(name)) => self.switch_agent(name),
            Effect::DecideQuit => self.decide_quit(),
            // Done once the loop ends: [`Effect::Quit`] follows.
            Effect::Leave(departure) => self.departure = Some(departure),
            Effect::NewSession => self.switch_conversation(None),
            Effect::ResumeSession(reference) => self.resume_session(reference),
            Effect::RequestResumePicker => self.show_resume_picker(),
            Effect::RequestModelPicker { refresh } => {
                let current = current(self.model.as_ref());
                self.bench.request_model_picker(refresh, current);
            }
            Effect::Remember(text) => self.bench.remember(&text, &self.linked()),
            Effect::OpenMemory(target) => self.bench.open_memory(target),
            Effect::RequestMemoryPicker => self
                .bench
                .memory_picker(self.config.remote.as_deref().map(|why| {
                    format!("/memory edits the daemon's workspace, which is not on this machine: {why}")
                })),
            Effect::MemoryEdited { target, path } => {
                let linked = self.linked();
                self.bench.memory_edited(target, &path, &linked);
            }
            Effect::RespondEscalation(id, answer) => self.respond_escalation(id, &answer),
            Effect::SetMode(mode) => self.set_mode(mode),
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
            Effect::RequestRulePicker => self.rule_picker(None),
            Effect::PickedRuleRemoval(n) => self.rule_picker(Some(n)),
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

    /// The agent as the workbench reaches it, over this connection.
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
fn current(shown: Option<&ShownModel>) -> Current<'_> {
    Current {
        endpoint: shown.map_or("", |shown| shown.endpoint.as_str()),
        model: shown.map_or("", |shown| shown.model.as_str()),
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
    use super::commands::{
        agent_entries, others_notice, report_lines, rule_picker, schedule_picker,
    };
    use super::turn::model_switch;
    use super::*;

    /// Attached on `link`.
    fn up(link: Link) -> Conn {
        Conn::Up {
            link,
            leaving: None,
        }
    }

    /// The `/schedules` picker: a row per schedule with its timing, next
    /// run, state, times fired and cost; Enter and `p` pause an active one
    /// and resume a paused one, `d` deletes after a confirmation, each by
    /// the schedule's id; with none, the line says the agent sets them.
    #[test]
    fn the_schedule_picker_rows_pause_resume_and_delete_by_id() {
        use crate::ui::{ModalKey, PickAct, PickerLine};
        use protocol::ScheduleOp;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let row = |id: &str, name: &str, state: &str| protocol::ScheduleRow {
            id: id.into(),
            name: name.into(),
            expression: "every 86400s from 2026-11-11 02:00:00Z".into(),
            next: (state == "active").then(|| "2026-11-12 02:00:00Z".into()),
            state: state.into(),
            fired: 1,
            cost: None,
        };
        let prices = crate::pricing::PriceTable::default();
        let mut picker = schedule_picker(
            &[
                row("s-1", "nightly", "active"),
                row("s-2", "weekly", "paused"),
            ],
            &prices,
        );
        assert_eq!(
            picker.rows[0].text,
            "nightly  every 86400s from 2026-11-11 02:00:00Z  next 2026-11-12 02:00:00Z  active  fired 1x  $0"
        );
        assert!(picker.rows[1].text.contains("next none  paused  fired 1x"));
        let press = |code| KeyEvent::from(code);
        let pause = ModalKey::Choose(PickAct::Schedule(ScheduleOp::Pause, "s-1".into()));
        assert_eq!(picker.key(&press(KeyCode::Enter)), pause);
        assert_eq!(picker.key(&press(KeyCode::Char('p'))), pause);
        picker.key(&press(KeyCode::Down));
        assert_eq!(
            picker.key(&press(KeyCode::Char('p'))),
            ModalKey::Choose(PickAct::Schedule(ScheduleOp::Resume, "s-2".into()))
        );
        assert_eq!(picker.key(&press(KeyCode::Char('d'))), ModalKey::Stay);
        assert!(
            matches!(&picker.line, Some(PickerLine::Confirm { question, .. }) if question.contains("delete schedule weekly?")),
            "{:?}",
            picker.line
        );
        assert_eq!(
            picker.key(&press(KeyCode::Char('y'))),
            ModalKey::Choose(PickAct::Schedule(ScheduleOp::Delete, "s-2".into()))
        );
        let none = schedule_picker(&[], &prices);
        assert!(none.rows.is_empty());
        assert!(
            none.empty.contains("meerkat_schedule_create"),
            "{}",
            none.empty
        );
    }

    /// The `/permissions` picker: a row per rule with its number, kind,
    /// tool, pattern or literal and layer; `d` asks before removing it by
    /// its number, saying for good for the workspace's own rules and until
    /// the daemon restarts for one written in another layer; Enter does
    /// nothing; with none, the empty line, and the hint says how to add.
    #[test]
    fn the_rule_picker_rows_remove_by_number_after_saying_for_how_long() {
        use crate::approval::desk::RuleRow;
        use crate::approval::rules::RuleOrigin;
        use crate::ui::{ModalKey, PickAct, PickerLine};
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let rule = |n, deny, arg: Option<&str>, literal, origin| RuleRow {
            n,
            deny,
            tool: "shell".into(),
            arg: arg.map(str::to_string),
            literal,
            origin,
        };
        let mut picker = rule_picker(&[
            rule(1, false, Some("git status*"), false, RuleOrigin::Realm),
            rule(4, true, Some("git push"), true, RuleOrigin::Global),
            rule(7, false, None, false, RuleOrigin::Cli),
        ]);
        let texts: Vec<&str> = picker.rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "1. allow shell pattern `git status*` · realm",
                "4. deny shell literal `git push` · global",
                "7. allow shell (every call) · cli",
            ]
        );
        let press = |code| KeyEvent::from(code);
        assert_eq!(picker.key(&press(KeyCode::Enter)), ModalKey::Stay);
        let question = |picker: &crate::ui::ListPicker| match &picker.line {
            Some(PickerLine::Confirm { question, .. }) => question.clone(),
            other => format!("not a question: {other:?}"),
        };
        picker.key(&press(KeyCode::Char('d')));
        assert!(
            question(&picker).contains("for good"),
            "{}",
            question(&picker)
        );
        assert_eq!(
            picker.key(&press(KeyCode::Char('y'))),
            ModalKey::Choose(PickAct::RemoveRule(1))
        );
        picker.key(&press(KeyCode::Down));
        picker.key(&press(KeyCode::Char('d')));
        let asked = question(&picker);
        assert!(
            asked.contains("global tui.toml") && asked.contains("until the daemon restarts"),
            "{asked}"
        );
        assert_eq!(
            picker.key(&press(KeyCode::Char('y'))),
            ModalKey::Choose(PickAct::RemoveRule(4))
        );
        picker.key(&press(KeyCode::Down));
        picker.key(&press(KeyCode::Char('d')));
        assert!(
            question(&picker).contains("command line"),
            "{}",
            question(&picker)
        );
        let none = rule_picker(&[]);
        assert!(none.rows.is_empty());
        assert!(none.footer.contains("/permissions add"), "{}", none.footer);
    }

    /// The start-up notice about the other agents, for a client that
    /// joined one and for one that got a new agent.
    #[test]
    fn the_start_up_notice_names_the_other_agents() {
        let row = |name: &str, running, clients| protocol::AgentRow {
            name: name.into(),
            session_id: None,
            running,
            clients,
            detached: clients == 0,
            schedules: 0,
        };
        let alone = [row("main-mine", Some(false), 1)];
        assert_eq!(others_notice(&alone, "main-mine", false), None);
        assert_eq!(others_notice(&alone, "main-mine", true), None);
        let rows = [
            row("main-abcd", Some(true), 0),
            row("main-mine", Some(false), 1),
            row("main-efgh", Some(false), 0),
            row("main-ijkl", None, 0),
            row("main-mnop", Some(false), 2),
        ];
        // Joined (`-c`, `--resume`): the others' state.
        let text = others_notice(&rows, "main-mine", false).unwrap();
        assert!(
            text.starts_with(
                "other agents work in this workspace: main-abcd is running, main-efgh is idle, main-ijkl is unknown, main-mnop is idle;"
            ),
            "{text}"
        );
        assert!(text.contains("/agents picks one"), "{text}");
        // A new agent: said so, with the others counted and how to reach
        // them; only an idle agent nobody works with counts as idle
        // without a client (running, unknown and attached ones do not).
        assert_eq!(
            others_notice(&rows, "main-mine", true).unwrap(),
            "started a new agent (main-mine); 4 others work here, 1 idle without a client: /agents picks one (rkat-tui -c joins the most recently active instead of starting a new one)"
        );
        let one = [
            row("main-mine", Some(false), 1),
            row("main-abcd", Some(true), 1),
        ];
        assert_eq!(
            others_notice(&one, "main-mine", true).unwrap(),
            "started a new agent (main-mine); 1 other works here: /agents picks one (rkat-tui -c joins the most recently active instead of starting a new one)"
        );
    }

    /// The picker's rows: state, the conversation's short id and title
    /// (from the conversation list), clients, the client's own agent
    /// marked, and why the retire key refuses an agent that is this
    /// client's, has a client, runs, or cannot be read.
    #[test]
    fn the_agent_picker_rows_have_state_conversation_clients_and_retire_refusals() {
        let session = SessionId::new();
        let other = SessionId::new();
        let row = |name: &str, session_id: Option<SessionId>, running, clients, detached| {
            protocol::AgentRow {
                name: name.into(),
                session_id,
                running,
                clients,
                detached,
                schedules: 0,
            }
        };
        let rows = vec![
            row("main-aaaa", Some(session.clone()), Some(false), 1, false),
            row("main-bbbb", Some(other.clone()), Some(false), 0, true),
            row("main-cccc", None, Some(true), 0, true),
            row("main-dddd", None, Some(false), 2, false),
            row("main-eeee", None, None, 0, true),
        ];
        let listed = vec![crate::daemon::conversations::Listed {
            session_id: other.clone(),
            agent: "main-bbbb".into(),
            in_use: None,
            last_used: 0,
            messages: Some(2),
            title: Some("fix the test".into()),
            current: false,
        }];
        let entries = agent_entries(&rows, &listed, Some("main-aaaa"));
        assert_eq!(entries.len(), 5);
        let (a, b) = (&entries[0], &entries[1]);
        assert!(a.mine && !b.mine);
        assert_eq!(
            a.conversation.as_deref(),
            Some(crate::text::short_id(&session.to_string()))
        );
        assert_eq!(a.title, None);
        assert_eq!(
            b.conversation.as_deref(),
            Some(crate::text::short_id(&other.to_string()))
        );
        assert_eq!(b.title.as_deref(), Some("fix the test"));
        assert_eq!((b.run, b.clients, b.detached), ("idle", 0, true));
        assert_eq!(b.unretirable, None, "idle and nobody's: retirable");
        let why = |i: usize| entries[i].unretirable.clone().unwrap();
        assert!(why(0).contains("/quit retires it"), "{}", why(0));
        assert!(why(2).contains("running a turn"), "{}", why(2));
        assert!(why(3).contains("has 2 clients"), "{}", why(3));
        assert_eq!(entries[4].run, "unknown");
        assert!(why(4).contains("cannot be read"), "{}", why(4));
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
                denials: crate::approval::desk::Denials {
                    consecutive: 1,
                    consecutive_limit: 3,
                    total: 2,
                    total_limit: 20,
                },
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
                denials: crate::approval::desk::Denials {
                    consecutive: 0,
                    consecutive_limit: 3,
                    total: 0,
                    total_limit: 20,
                },
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
            crate::daemon::handshake::acknowledge(
                &mut server_write,
                &pending,
                None,
                None,
                None,
                timeout,
            )
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
            }),
            why: Shown::Others,
            warning: None,
        }
    }

    /// The finding this guards: a switch read on a connection lost before
    /// the reattach installed that connection's stream id, and every
    /// agent event after was dropped as another stream's.
    #[tokio::test]
    async fn a_switch_read_on_a_lost_connection_is_not_installed_after_the_reattach() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut ui_rx) = runner(dir.path());
        let live = SessionId::new();
        runner.conn = up(link("live", live.clone()).await);
        runner.link_epoch = 2;
        let other = SessionId::new();
        runner.switching = true;
        runner.on_report(1, switched(&other, "dead"));
        let link_now = runner.conn.link().unwrap();
        assert_eq!(link_now.stream, "live");
        assert_eq!(link_now.session, live);
        assert!(runner.switching, "a switch started since still runs");
        while let Ok(msg) = ui_rx.try_recv() {
            assert!(!matches!(msg, Msg::ResetTranscript(_)), "{msg:?}");
        }
        // The same report of the current link is shown.
        runner.on_report(2, switched(&other, "next"));
        let link_now = runner.conn.link().unwrap();
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
            runner.conn = up(link);
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
            assert_eq!(runner.conn.stopping(), ended, "{case}");
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
            runner.conn = up(link);
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
            assert!(!runner.conn.stopping(), "{remote}");
        }
        // Its endpoint file gone is the daemon's own evidence, by address too.
        let (mut runner, mut ui_rx) = runner(dir.path());
        let mut link = link_held("s", SessionId::new(), false).await;
        link.endpoint.address = closed_address;
        std::fs::remove_file(dir.path().join("mob.endpoint")).unwrap();
        runner.config.address = Some(link.endpoint.clone());
        runner.conn = up(link);
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
        runner.conn = up(link("live", SessionId::new()).await);
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
            // Not `async`: it cannot wait on the daemon.
            runner.handle_effect(effect);
        }
        assert!(runner.turn.active, "the unanswered prompt's turn runs");
        assert!(
            runner.turn.interrupted_at.is_some(),
            "Esc interrupted it at once"
        );
        // The steer is expected at the next boundary while its send runs.
        assert!(runner.steers.holds(PendingId(1)));
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
        runner.conn = up(link("live", SessionId::new()).await);
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
            runner.steers.sending(PendingId(id));
        }
        // 2's input joined the run before its send answered.
        assert_eq!(runner.steers.applied(&early), None);
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
        assert!(runner.steers.holds(PendingId(1)));
        assert!(!runner.steers.holds(PendingId(3)));
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

    /// Events and idle notices fed to a runner without a connection.
    struct Feed {
        session: SessionId,
        seq: u64,
    }

    impl Feed {
        fn envelope(&mut self, payload: AgentEvent) -> EventEnvelope<AgentEvent> {
            self.seq += 1;
            EventEnvelope::new("test", self.seq, None, payload)
        }

        fn started(&mut self, run: &RunId) -> EventEnvelope<AgentEvent> {
            use meerkat_core::types::{ContentInput, RunInput, TranscriptMessageIdentity};
            self.envelope(AgentEvent::RunStarted {
                session_id: self.session.clone(),
                identity: TranscriptMessageIdentity {
                    run_id: Some(run.clone()),
                    ..TranscriptMessageIdentity::default()
                },
                input: RunInput::Content {
                    content: ContentInput::Text("x".into()),
                },
                request_reasoning: None,
            })
        }

        fn completed(&mut self, run: &RunId) -> EventEnvelope<AgentEvent> {
            use meerkat_core::types::TranscriptMessageIdentity;
            self.envelope(AgentEvent::RunCompleted {
                session_id: self.session.clone(),
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
            })
        }

        fn applied(&mut self, input: &InputId) -> EventEnvelope<AgentEvent> {
            self.envelope(AgentEvent::BoundaryAppendApplied {
                run_id: RunId::new(),
                input_id: input.clone(),
                content: meerkat_core::types::ContentInput::Text("steer".into()),
                append_count: 1,
                notices: Vec::new(),
                transcript_start: None,
            })
        }

        fn idle(&self, last_run: &RunId, held: usize) -> protocol::IdleNote {
            protocol::IdleNote {
                agent: "a".into(),
                session_id: self.session.clone(),
                last_run: Some(last_run.clone()),
                held,
            }
        }
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<Msg>) -> Vec<Msg> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn turn_ends(msgs: &[Msg]) -> usize {
        msgs.iter()
            .filter(|m| matches!(m, Msg::TurnFinished(_)))
            .count()
    }

    /// A turn ends on the daemon's idle notice naming a run it saw end,
    /// in either order of the notice and the run's end; a notice of an
    /// earlier run ends nothing, and neither does a run's end alone.
    #[tokio::test]
    async fn a_turn_ends_on_the_idle_notice_after_a_run_it_saw_end() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut rx) = runner(dir.path());
        let mut feed = Feed {
            session: SessionId::new(),
            seq: 0,
        };
        let (earlier, first, follow_up) = (RunId::new(), RunId::new(), RunId::new());
        runner.idle_reported(feed.idle(&earlier, 0));
        runner.turn = Turn {
            active: true,
            ..Turn::default()
        };
        runner.on_event(feed.started(&first), None);
        runner.on_event(feed.completed(&first), None);
        assert!(runner.turn.active, "the stale notice ends nothing");
        // Another client's steer runs as a follow-up run: still the turn.
        runner.on_event(feed.started(&follow_up), None);
        runner.idle_reported(feed.idle(&first, 0));
        assert!(runner.turn.active, "a run is open");
        // The notice may come before the run's end shows here.
        runner.idle_reported(feed.idle(&follow_up, 0));
        assert!(runner.turn.active);
        assert_eq!(turn_ends(&drain(&mut rx)), 0);
        runner.on_event(feed.completed(&follow_up), None);
        assert!(!runner.turn.active);
        assert_eq!(turn_ends(&drain(&mut rx)), 1);
    }

    /// A steer whose send is unanswered keeps the turn open past the idle
    /// notice (it may start a run of its own); a steer the agent still
    /// holds at the turn's end is not shown delivered then, and is marked
    /// when the run carrying it starts, or its own boundary event comes,
    /// in a later turn. Both orders.
    #[tokio::test]
    async fn a_steer_the_agent_has_not_read_stays_followed_across_turns() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut rx) = runner(dir.path());
        let mut feed = Feed {
            session: SessionId::new(),
            seq: 0,
        };
        for by_boundary in [false, true] {
            let (run, later) = (RunId::new(), RunId::new());
            let input = InputId::new();
            runner.turn = Turn {
                active: true,
                ..Turn::default()
            };
            runner.on_event(feed.started(&run), None);
            runner.steers.sending(PendingId(7));
            runner.on_event(feed.completed(&run), None);
            runner.idle_reported(feed.idle(&run, 0));
            assert!(runner.turn.active, "its send is unanswered");
            runner.sent(0, Some(PendingId(7)), Ok(Some(input.clone())));
            // The daemon decided before the steer arrived; it holds it now.
            assert!(!runner.turn.active, "nothing on its way any more");
            let msgs = drain(&mut rx);
            assert_eq!(turn_ends(&msgs), 1);
            assert!(
                !msgs.iter().any(|m| matches!(m, Msg::SteerDelivered(_))),
                "not read yet: {msgs:?}"
            );
            assert!(runner.steers.holds(PendingId(7)));
            if by_boundary {
                // A turn of another client's runs; the steer joins it at a
                // boundary.
                runner.on_event(feed.applied(&input), None);
            } else {
                // The agent runs it as a run of its own.
                runner.on_event(feed.started(&later), None);
            }
            let msgs = drain(&mut rx);
            assert_eq!(
                msgs.iter()
                    .filter(|m| matches!(m, Msg::SteerDelivered(PendingId(7))))
                    .count(),
                1,
                "{msgs:?}"
            );
            if !by_boundary {
                // Its own run shows no second copy of its text.
                assert!(
                    msgs.iter()
                        .any(|m| matches!(m, Msg::RemoteTurn { prompt: None })),
                    "{msgs:?}"
                );
            }
            assert!(!runner.steers.holds(PendingId(7)));
            runner.finish_turn();
            drain(&mut rx);
        }
    }

    /// An idle notice with nothing held while this client still follows
    /// admitted steers asks the daemon what became of them (once at a
    /// time; a notice meanwhile asks again on the answer); one held asks
    /// nothing. The answer decides each steer by its state: written into
    /// a run is delivered, ended without one returns to the editor with
    /// why, still held or not found stays followed.
    #[tokio::test]
    async fn an_idle_agent_holding_nothing_settles_the_followed_steers_by_their_state() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut rx) = runner(dir.path());
        let session = SessionId::new();
        runner.conn = up(link("live", session.clone()).await);
        let feed = Feed { session, seq: 0 };
        let inputs: Vec<InputId> = (0..4).map(|_| InputId::new()).collect();
        for (n, input) in (1..).zip(&inputs) {
            runner.steers.sending(PendingId(n));
            runner.sent(0, Some(PendingId(n)), Ok(Some(input.clone())));
        }
        drain(&mut rx);
        let run = RunId::new();
        runner.idle_reported(feed.idle(&run, 1));
        assert_eq!(runner.asking_fates, None, "the runtime still holds one");
        runner.idle_reported(feed.idle(&run, 0));
        assert_eq!(runner.asking_fates, Some(false));
        runner.idle_reported(feed.idle(&run, 0));
        assert_eq!(runner.asking_fates, Some(true), "asked again on the answer");
        let row = |n: usize, fate| protocol::InputRow {
            input_id: inputs[n].clone(),
            fate,
        };
        runner.input_fates(Ok(protocol::InputFates {
            inputs: vec![
                row(0, protocol::InputFate::Taken),
                row(
                    1,
                    protocol::InputFate::Dropped {
                        why: Some("it was cancelled".into()),
                    },
                ),
                row(2, protocol::InputFate::Pending),
                row(3, protocol::InputFate::Unknown),
            ],
        }));
        let msgs = drain(&mut rx);
        assert!(
            matches!(
                msgs.as_slice(),
                [
                    Msg::SteerDelivered(PendingId(1)),
                    Msg::NotDelivered { id: PendingId(2), why: Some(why) },
                ] if why == "it was cancelled"
            ),
            "{msgs:?}"
        );
        assert!(!runner.steers.holds(PendingId(1)));
        assert!(!runner.steers.holds(PendingId(2)));
        assert!(runner.steers.holds(PendingId(3)) && runner.steers.holds(PendingId(4)));
        assert!(
            !runner.admitted.contains(&PendingId(2)),
            "not saved as sent"
        );
        assert!(runner.admitted.contains(&PendingId(1)));
        // The notice that came meanwhile asked again, for the two left.
        assert_eq!(runner.asking_fates, Some(false));
        // A failed question leaves them followed; nothing is asked again
        // until the next notice.
        runner.input_fates(Err("lost".into()));
        assert_eq!(runner.asking_fates, None);
        assert!(drain(&mut rx).is_empty());
        assert!(runner.steers.holds(PendingId(3)));
        // The answer to a question for a steer settled since marks nothing.
        runner.input_fates(Ok(protocol::InputFates {
            inputs: vec![row(0, protocol::InputFate::Taken)],
        }));
        assert!(drain(&mut rx).is_empty());
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

    /// An interrupt the daemon does not confirm in time is followed by a
    /// hard cancel of the agent's current run and a second wait; only
    /// then does the turn end, as unconfirmed (never called interrupted).
    #[tokio::test]
    async fn an_unconfirmed_interrupt_escalates_once_then_ends_unconfirmed() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut ui_rx) = runner(dir.path());
        runner.conn = up(link("live", SessionId::new()).await);
        runner.turn = Turn {
            active: true,
            interrupted_at: Some(Instant::now()),
            give_up_at: Some(Instant::now()),
            ..Turn::default()
        };
        runner.on_deadline();
        assert!(runner.turn.active, "it waits once more");
        assert!(runner.turn.escalated);
        assert!(runner.turn.give_up_at.is_some_and(|at| at > Instant::now()));
        let notices: Vec<String> = std::iter::from_fn(|| ui_rx.try_recv().ok())
            .filter_map(|msg| match msg {
                Msg::Notice { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert!(
            notices
                .iter()
                .any(|t| t.contains("cancelling whatever the agent runs now")),
            "{notices:?}"
        );
        runner.turn.give_up_at = Some(Instant::now());
        runner.on_deadline();
        assert!(!runner.turn.active);
        let ended = std::iter::from_fn(|| ui_rx.try_recv().ok())
            .find(|msg| matches!(msg, Msg::TurnFinished(_)));
        assert!(
            matches!(
                ended,
                Some(Msg::TurnFinished(Err(TurnFailure::Unconfirmed)))
            ),
            "{ended:?}"
        );
    }

    #[test]
    fn only_a_stopped_receipt_counts_as_a_stop() {
        use serde_json::json;
        assert!(remote::stopped(
            &json!({"receipt": {"outcome": "stopped", "run_id": "r", "contributors": []}})
        ));
        for outcome in ["not_current", "not_stoppable"] {
            assert!(!remote::stopped(
                &json!({"receipt": {"outcome": outcome, "run_id": "r"}})
            ));
        }
        assert!(!remote::stopped(&json!({})));
    }
}
