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

use meerkat_core::lifecycle::RunId;
use meerkat_core::{AgentErrorClass, AgentEvent, EventEnvelope, Message, SessionId};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::ClientError;
use super::approvals::{Approvals, decision_params};
use super::catchup::{CursorFile, Mark};
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

mod btw;
mod commands;
mod conn;
mod depart;
mod link;
mod steers;
mod switch;
mod turn;
mod wires;

use conn::{Conn, ConnEvent, Leaving};
use turn::Turn;

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

/// Presses of Esc this close to each other are one interrupt: once its
/// run has ended they say so once and act on nothing.
pub const INTERRUPT_REPEAT: Duration = crate::ui::INTERRUPT_REPEAT;

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
    /// Where `/wire` looks for the other workspaces' daemons (`None`: every
    /// endpoint file `--list` finds; tests name their own).
    pub wire_files: Option<Vec<PathBuf>>,
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

/// A mob's stream, opened.
struct OpenedMob {
    /// The stream.
    stream: String,
    /// Its members, each with its session when it could be read.
    members: Vec<(String, Option<SessionId>)>,
    /// The workspace's mob's: the helpers seated for this conversation
    /// (each by its labels), as the daemon listed them.
    seated: Option<Vec<Vec<String>>>,
}

/// What a background task reports.
enum Report {
    /// A `/model` switch (`rkat/set_model`) applied, or why not.
    /// The classifier's model (`rkat/report`), for its picker.
    ClassifierModel(Option<String>),
    ModelSet {
        switch: ModelSwitch,
        result: Result<protocol::ModelSet, String>,
    },
    /// The daemon answered an interrupt (`rkat/interrupt`): of the turn's
    /// interrupt `epoch` ([`turn::state::Interrupt::epoch`]), or of a press with no
    /// turn open (`None`).
    Interrupted {
        epoch: Option<u64>,
        result: Result<protocol::Interrupted, String>,
    },
    /// The daemon answered the `/btw` numbered `id`.
    Btw {
        id: u64,
        result: Result<String, String>,
    },
    /// The daemon answered `/compact`'s request ([`Runner::compact_asked`]).
    CompactAsked(Result<(), String>),
    /// The mobs the daemon hosts (listed once per attach and per
    /// conversation; later ones are pushed, [`protocol::MOB`]).
    Mobs(Vec<String>),
    /// A mob's stream opened, with its members' sessions; or not.
    MobOpened {
        mob: String,
        opened: Result<OpenedMob, String>,
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
        result: Result<Option<remote::Admitted>, SendFailure>,
        /// The text to send once more: the daemon reloaded the agent
        /// after a storage failure and the input was not taken
        /// ([`protocol::AGENT_RELOADED`]).
        again: Option<String>,
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

/// Why a prompt or a steer is not shown sent ([`Report::Sent`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SendFailure {
    /// The agent does not have it: the daemon or the engine refused it,
    /// or it never reached the daemon (why, in words).
    NotSent(String),
    /// Nobody can say: the answer was lost and the daemon could not be
    /// asked, or had no answer of the engine's yet. It may have reached
    /// the agent.
    Unknown(String),
}

/// A prompt or a steer for the agent, sent by [`Runner::outbound`]'s task
/// in the order they were made.
struct Outbound {
    connection: Arc<Connection>,
    text: String,
    /// `Some`: a steer.
    steer: Option<PendingId>,
    /// Sent behind the agent's runs (the engine's `queue`): only the quiet
    /// run `/compact` starts, which must be a run of its own. Everything
    /// the user sends is a `steer`, a prompt too: it goes in at the
    /// agent's next model request, or starts a run when none goes, and
    /// never waits behind a run that started meanwhile (a helper's or a
    /// peer's message, a note of the daemon's).
    queue: bool,
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
    /// The daemon reloaded the agent after a storage failure
    /// ([`protocol::RELOADED`]): the same conversation, its stream ended
    /// with the old registration.
    Reloaded,
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

/// The background tasks' reports, each stamped with the link it started
/// on.
struct Reports {
    tx: mpsc::UnboundedSender<(u64, Report)>,
    rx: mpsc::UnboundedReceiver<(u64, Report)>,
    /// Counts the links made: a report started on an earlier link (a
    /// connection lost since) names a stream or a session of that
    /// connection and is not applied ([`Runner::on_report`]).
    epoch: u64,
}

/// The agent's helpers as this client follows them.
#[derive(Default)]
struct Helpers {
    /// The helpers and their views.
    followers: Followers,
    /// Events of streams opened but not registered yet, by stream id.
    orphans: Orphans<meerkat_mob::AttributedEvent>,
    /// The mobs the daemon said host helpers (listed at the attach,
    /// pushed since): one is opened once it is this agent's
    /// ([`Followers::should_open`], which a helper's session or a council
    /// can make true later).
    known_mobs: HashSet<String>,
}

impl Helpers {
    /// The connection is gone: the followers lose their streams, and the
    /// events held for streams and the mobs known go with it.
    fn connection_lost(&mut self) {
        self.followers.streams_lost();
        self.orphans.clear();
        self.known_mobs.clear();
    }
}

/// The client's runner: the loop between the UI and the daemon.
///
/// Each concern holds its own fields: the turn ([`Turn`], and
/// [`turn::Turns`] for what outlives one), the steers
/// ([`steers::Steering`]), the model ([`turn::Models`]), the connection
/// ([`Conn`]), where the events shown stand ([`link::Seen`]), the
/// helpers, the switches ([`switch::Switches`]), `/btw`
/// ([`btw::Btws`]) and leaving ([`depart::Departing`]). What stays here
/// is shared across them: the configuration, the bound agent, the UI's
/// channels, the reports, the dialogs, the workbench and the held
/// prompts.
pub struct Runner {
    config: ClientConfig,
    /// The agent the daemon bound this client to (`None` before the first
    /// attach); a reconnect asks for it again.
    agent: Option<String>,
    ui_tx: mpsc::UnboundedSender<Msg>,
    effect_rx: mpsc::UnboundedReceiver<Effect>,
    /// The connection's lifecycle.
    conn: Conn,
    reports: Reports,
    turn: Turn,
    turns: turn::Turns,
    steering: steers::Steering,
    models: turn::Models,
    seen: link::Seen,
    /// The approval dialogs shown.
    approvals: Approvals,
    helpers: Helpers,
    switches: switch::Switches,
    btw: btw::Btws,
    /// `!` shell mode, `/remember`, `/memory`, `/export`, prices, the
    /// model lists and unsent messages.
    bench: Workbench,
    /// Prompts sent before the first attach finished (the command line's
    /// prompt is sent as the UI opens) or while a conversation switch
    /// runs: held until the link is up and no switch runs, or the first
    /// connect failed; `None` once released.
    held: Option<Vec<Effect>>,
    departing: depart::Departing,
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

    /// State what a picker's act did (`done`, when it did something) under
    /// the rows of the picker of `kind` (sent after its refreshed list).
    fn picker_done(&self, kind: crate::ui::PickerKind, done: Option<String>) {
        if let Some(text) = done {
            let _ = self.ui_tx.send(Msg::PickerDone { kind, text });
        }
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
            ui_tx,
            effect_rx,
            conn: Conn::Idle,
            reports: Reports {
                tx: reports_tx,
                rx: reports_rx,
                epoch: 0,
            },
            turn: Turn::default(),
            turns: turn::Turns::default(),
            steering: steers::Steering::default(),
            models: turn::Models::default(),
            seen: link::Seen::default(),
            approvals: Approvals::default(),
            helpers: Helpers::default(),
            switches: switch::Switches::default(),
            btw: btw::Btws::default(),
            bench,
            held: Some(Vec::new()),
            departing: depart::Departing::default(),
        }
    }

    /// What the runner leaves to say on the terminal once the UI is gone
    /// (whether the daemon keeps running).
    #[must_use]
    pub fn farewells(&self) -> Farewells {
        Arc::clone(&self.departing.farewells)
    }

    /// Say `text` now and again on the terminal after the UI closed.
    fn farewell(&self, level: NoticeLevel, text: String) {
        self.say().farewell(level, text);
    }

    /// What a task says to the UI, and on the terminal after it closed.
    fn say(&self) -> Say {
        Say {
            ui_tx: self.ui_tx.clone(),
            farewells: Arc::clone(&self.departing.farewells),
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
                    Some(effect) => {
                        if let Some(effect) = self.unless_held(effect) {
                            self.handle_effect(effect);
                        }
                    }
                },
                event = self.conn.next() => match event {
                    ConnEvent::Note(note) => self.on_notification(note),
                    ConnEvent::Lost => self.lost(),
                    ConnEvent::Connected(joined) => self.connected(joined),
                    ConnEvent::RetryDue => self.retry(),
                },
                Some((epoch, report)) = self.reports.rx.recv() => self.on_report(epoch, report),
                () = sleep_until(self.turn.deadline()) => self.on_deadline(),
                joined = self.bench.background() => self.model_lists(joined),
            }
        }
        if let Some(departure) = self.departing.chosen.take() {
            self.depart(departure).await;
        }
        self.save_cursor();
        self.bench.cancel_shell();
        if let Conn::Connecting { task, .. } = &self.conn {
            task.abort();
        }
        self.bench.save_unsent(&self.steering.admitted)
    }

    fn send(&self, msg: Msg) {
        let _ = self.ui_tx.send(msg);
    }

    /// `effect` to serve now, or `None` when it waits ([`Self::releasable`]
    /// lets it go, in the order it came): a prompt until the first link
    /// is up, and while a switch runs (it goes to the conversation the
    /// switch ends on, over its stream); a steer while a prompt waits so,
    /// since it steers that prompt's turn (served now it would find no
    /// turn and be refused, and the UI would hold it for that turn's end).
    fn unless_held(&mut self, effect: Effect) -> Option<Effect> {
        let hold = match &effect {
            Effect::StartTurn(_) => {
                self.held.is_some() || self.switches.running || self.models.switching
            }
            Effect::Steer { .. } => self
                .held
                .as_ref()
                .is_some_and(|held| held.iter().any(|e| matches!(e, Effect::StartTurn(_)))),
            _ => false,
        };
        if hold {
            self.held.get_or_insert_with(Vec::new).push(effect);
            return None;
        }
        Some(effect)
    }

    /// The held prompts, once they may go: the first link is up and no
    /// start-up switch runs, or the first connect failed.
    fn releasable(&mut self) -> Vec<Effect> {
        let ready = matches!(self.conn, Conn::Failed)
            || (self.conn.link().is_some() && !self.switches.running && !self.models.switching);
        if ready {
            self.held.take().unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// The channel a background task started now reports on.
    fn reporter(&self) -> Reporter {
        Reporter {
            tx: self.reports.tx.clone(),
            epoch: self.reports.epoch,
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
        if epoch != self.reports.epoch {
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
                again: Some(text),
                ..
            } => self.send_after_reload(turn, steer, text),
            Report::Sent {
                turn,
                steer,
                result,
                again: None,
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
            Report::ClassifierModel(label) => {
                let named = label
                    .as_deref()
                    .and_then(crate::endpoints::EndpointRef::parse);
                let shown = self
                    .models
                    .shown
                    .as_ref()
                    .map(|m| m.endpoint.clone())
                    .unwrap_or_default();
                let current = match (&named, label.as_deref()) {
                    (Some(named), _) => Current {
                        endpoint: &named.endpoint,
                        model: &named.model,
                    },
                    // A bare model (the start-up endpoint's) is marked on
                    // the agent's endpoint.
                    (None, Some(model)) => Current {
                        endpoint: &shown,
                        model,
                    },
                    (None, None) => Current {
                        endpoint: "",
                        model: "",
                    },
                };
                self.bench.request_model_picker(false, current, true);
            }
            Report::Rehomed(rehomed) => self.rehomed(*rehomed),
            Report::ModelSet { switch, result } => {
                self.models.switching = false;
                self.model_set(switch, result);
            }
            Report::CompactAsked(asked) => self.compact_asked(asked),
            Report::Interrupted { epoch, result } => self.interrupt_answered(epoch, result),
            Report::Btw { id, result } => self.btw_answered(id, result),
            Report::Mobs(mobs) => {
                self.helpers.known_mobs.extend(mobs);
                self.open_known_mobs();
            }
            Report::MobOpened { mob, opened } => match opened {
                Ok(OpenedMob {
                    stream,
                    members,
                    seated,
                }) => {
                    if self.conn.link().is_none() {
                        return;
                    }
                    if let Some(seated) = &seated {
                        self.seated_helpers(seated);
                    }
                    self.helpers.followers.opened(stream.clone(), &mob);
                    let mut actions = Vec::new();
                    for event in self.helpers.orphans.take(&stream) {
                        actions.extend(self.helpers.followers.event(&stream, event));
                    }
                    for (label, session) in members {
                        actions.extend(self.helpers.followers.member(&mob, &label, session));
                    }
                    self.follow(actions);
                    // A helper's session may make a mob this agent's.
                    self.open_known_mobs();
                }
                Err(why) => {
                    tracing::debug!(%mob, %why, "cannot follow the mob");
                    self.helpers.followers.open_failed(&mob);
                }
            },
            Report::Backfilled { helper, events } => {
                let actions = self.helpers.followers.backfilled(&helper, events);
                self.follow(actions);
            }
            Report::HelperModel { helper, model } => {
                let actions = self.helpers.followers.model(&helper, model);
                self.follow(actions);
            }
            Report::Switched {
                result,
                why: shown,
                warning,
            } => {
                self.switches.running = false;
                match result {
                    Ok(adopted) => {
                        self.adopted(adopted, shown, warning);
                        self.follow_pushed_session();
                        // The stream is open again: the input the reload
                        // turned back goes once more.
                        if let Some((turn, steer, text)) = self.switches.again.take() {
                            self.send_again(turn, steer, text);
                        }
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
                self.switches.running = false;
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
            Effect::ReadCompactionSummary => self.read_compaction_summary(),
            Effect::Btw {
                question,
                running_tool,
                current_turn,
            } => self.ask_btw(question, running_tool, current_turn),
            Effect::CancelBtw => self.cancel_btw(),
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
            Effect::Rehome(reference) => self.rehome(reference, None),
            Effect::RehomeDiscarding { argument, discard } => {
                self.rehome(Some(argument), Some(discard));
            }
            Effect::RehomeCheck(typed) => self.rehome_check(typed),
            Effect::RemoveState(id) => self.remove_state(id),
            Effect::DaemonStart => self.daemon_start(),
            Effect::Agents(None) => self.show_agent_picker(),
            Effect::NewAgent => self.new_agent(),
            Effect::RetireAgent(name) => self.retire_agent(name),
            Effect::StopHelper(path) => self.stop_helper(path),
            Effect::Audit(lines) => self.show_audit(lines),
            Effect::Schedules(action) => self.schedules(action),
            Effect::PickedSchedule(op, id) => self.picked_schedule(op, id),
            Effect::Agents(Some(name)) => self.switch_agent(name),
            Effect::Wire(target) => self.wire(target),
            Effect::WireTo { workspace, agent } => self.wire_to(workspace, agent),
            Effect::RemoveWire { workspace, agent } => self.remove_wire(workspace, agent),
            Effect::DecideQuit => self.decide_quit(),
            // Done once the loop ends: [`Effect::Quit`] follows.
            Effect::Leave(departure) => self.departing.chosen = Some(departure),
            Effect::NewSession => self.switch_conversation(None),
            Effect::ResumeSession(reference) => self.resume_session(reference),
            Effect::RequestResumePicker => self.show_resume_picker(),
            Effect::SetSettings { changes, user } => self.set_settings(changes, user),
            Effect::RequestSettingsPicker(filter) => self.settings_picker(filter, None),
            Effect::PickedSetting {
                key,
                value,
                user,
                filter,
            } => {
                self.settings_picker(filter, Some((key, value, user)));
            }
            Effect::CompactPrepareShow => self.compact_prepare_show(),
            Effect::CompactPrepareStatus => {
                self.outcome_command::<protocol::method::CompactStatus>(
                    protocol::Empty {},
                    "/compact status",
                );
            }
            Effect::DroppedCommands(texts) => self.farewell(
                NoticeLevel::Warn,
                format!(
                    "dropped what waited for the turn's end: {}",
                    texts.join(", ")
                ),
            ),
            Effect::RequestModelPicker { refresh } => {
                let current = current(self.models.shown.as_ref());
                self.bench.request_model_picker(refresh, current, false);
            }
            Effect::RequestClassifierPicker => self.classifier_picker(),
            Effect::SetClassifierModel(reference, user) => {
                self.set_classifier_model(&reference, user);
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
                user,
            } => {
                let command = RuleCommand::Add {
                    deny: effect == RuleEffect::Deny,
                    tool,
                    pattern,
                    user,
                    literal: false,
                };
                self.outcome_command::<protocol::method::Rules>(command, "/permissions");
            }
            Effect::RemoveRule(n) => {
                self.outcome_command::<protocol::method::Rules>(
                    RuleCommand::Remove { n },
                    "/permissions",
                );
            }
            Effect::RequestPermissions(open) => self.permissions(None, open),
            Effect::PickedRuleRemoval(n) => {
                self.permissions(Some(commands::Picked::RemoveRule(n)), None);
            }
            Effect::PickedGrantRemoval(id) => {
                self.permissions(Some(commands::Picked::RemoveGrant(id)), None);
            }
            Effect::PickedStatementRemoval(id) => {
                self.permissions(Some(commands::Picked::RemoveStatement(id)), None);
            }
            Effect::PickedStatement { scope, text } => {
                self.permissions(Some(commands::Picked::AddStatement(text, scope)), None);
            }
            Effect::PickedRule(form) => {
                self.permissions(Some(commands::Picked::AddRule(form)), None);
            }
            Effect::PickedRuleEdit { n, old, new } => {
                self.permissions(Some(commands::Picked::EditRule { n, old, new }), None);
            }
            Effect::AutoStats => {
                self.outcome_command::<protocol::method::AutoStats>(
                    protocol::Empty {},
                    "/auto stats",
                );
            }
            Effect::ClassifierCheck(selection) => self.classifier_check(selection),
            Effect::PickedApproval(id, scope, effect) => {
                self.permissions(Some(commands::Picked::Approve(id, scope, effect)), None);
            }
            Effect::PickedWithdrawal(id) => {
                self.permissions(Some(commands::Picked::Withdraw(id)), None);
            }
            Effect::PickedHostTrust { host, user } => {
                let params = protocol::TrustHostParams::Host { host, user };
                self.permissions(Some(commands::Picked::TrustHost(params)), None);
            }
            Effect::PickedCallHostTrust { id, user } => {
                let params = protocol::TrustHostParams::Call { id, user };
                self.permissions(Some(commands::Picked::TrustHost(params)), None);
            }
            Effect::PickedHostRemoval(host) => {
                self.permissions(Some(commands::Picked::UntrustHost(host)), None);
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
        agent_entries, others_notice, report_lines, schedule_picker, settings_picker,
    };
    use super::steers::FatesAsk;
    use super::turn::model_switch;
    use super::turn::state::{Deadline, Interrupt, Kind, Open};
    use super::*;
    use meerkat_core::lifecycle::InputId;

    /// A send admitted by the engine as `input`.
    fn engine(input: InputId) -> Option<remote::Admitted> {
        Some(remote::Admitted { input })
    }

    /// The runner's open turn.
    fn open(runner: &Runner) -> &Open {
        runner.turn.open_ref().expect("a turn is open")
    }

    /// The runner's open turn, to change.
    fn open_mut(runner: &mut Runner) -> &mut Open {
        runner.turn.open_mut().expect("a turn is open")
    }

    /// A turn of this client's prompt, the UI's turn `id`.
    fn prompted(id: u64) -> Turn {
        Turn::prompt(id, "x".into())
    }

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
            matches!(&picker.line, Some(PickerLine::Confirm { question, .. }) if question.contains("Delete schedule weekly?")),
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

    /// `/settings`: one row per setting (key, value, layer), filtered by a
    /// substring; Enter opens the workspace's field, `u` the user's (Tab
    /// switches), Enter writes it; `d` asks before removing the value at
    /// the layer in force and says why when rkat-tui does not write it.
    #[test]
    fn the_settings_picker_edits_either_level_and_removes_the_one_in_force() {
        use crate::config::registry::{Applies, Level};
        use crate::daemon::protocol::{SectionRow, SettingRow, SettingsList};
        use crate::ui::{ModalKey, PickAct, PickerLine};
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let row = |key: &str, value: &str, source, levels: Vec<(Level, String)>| SettingRow {
            key: key.into(),
            about: "what it does".into(),
            takes: "a whole number".into(),
            applies: Applies::Live,
            value: value.into(),
            source,
            levels,
            default: "1".into(),
            project: "may only lower it".into(),
            command: None,
        };
        let list = SettingsList {
            rows: vec![
                row(
                    "helpers.max_depth",
                    "2",
                    Level::User,
                    vec![(Level::User, "2".into())],
                ),
                row(
                    "search.max_entries",
                    "10",
                    Level::Project,
                    vec![(Level::Project, "10".into())],
                ),
                row("ui.bell", "true", Level::Default, vec![]),
            ],
            sections: vec![SectionRow {
                key: "endpoints.<name>".into(),
                about: "endpoints".into(),
                project: "may not set it".into(),
                applies: "at the next start".into(),
                flags: "--endpoint-def".into(),
                commands: "—".into(),
            }],
            user_file: "/u/tui.toml".into(),
            workspace_file: "/r/tui.toml".into(),
        };
        let all = settings_picker(&list, None);
        assert_eq!(all.rows.len(), 4);
        assert_eq!(all.rows[0].text, "helpers.max_depth  2  (user)");
        assert!(
            all.rows[0]
                .note
                .as_deref()
                .unwrap()
                .starts_with("user 2 · workspace — · command line — · project — · default 1; a change applies at once"),
            "{:?}",
            all.rows[0].note
        );
        assert!(all.footer.contains("/u/tui.toml") && all.footer.contains("/r/tui.toml"));
        let mut picker = settings_picker(&list, Some("helpers"));
        assert_eq!(picker.rows.len(), 1);
        let press = |code| KeyEvent::from(code);
        // `u`: the user's value, Tab to the workspace's, typed, written.
        assert_eq!(picker.key(&press(KeyCode::Char('u'))), ModalKey::Stay);
        assert!(matches!(
            &picker.line,
            Some(PickerLine::Field(crate::ui::Field { what: crate::ui::FieldFor::Setting { user: true, .. }, text })) if text == "2"
        ));
        picker.key(&press(KeyCode::Backspace));
        picker.type_text("3");
        picker.key(&press(KeyCode::Tab));
        assert_eq!(
            picker.key(&press(KeyCode::Enter)),
            ModalKey::Choose(PickAct::SetSetting {
                key: "helpers.max_depth".into(),
                value: Some("3".into()),
                user: false,
                filter: Some("helpers".into()),
            })
        );
        // `d` removes the user's (where the value in force comes from).
        picker.key(&press(KeyCode::Char('d')));
        assert!(
            matches!(&picker.line, Some(PickerLine::Confirm { question, .. }) if question.contains("your tui.toml"))
        );
        assert_eq!(
            picker.key(&press(KeyCode::Char('y'))),
            ModalKey::Choose(PickAct::SetSetting {
                key: "helpers.max_depth".into(),
                value: None,
                user: true,
                filter: Some("helpers".into()),
            })
        );
        // A project's value is not rkat-tui's to remove; the default has
        // nothing to remove.
        let mut picker = settings_picker(&list, Some("search"));
        picker.key(&press(KeyCode::Char('d')));
        assert!(
            matches!(&picker.line, Some(PickerLine::Refused(why)) if why.contains("the project's .rkat/tui.toml sets it"))
        );
        let mut picker = settings_picker(&list, Some("ui.bell"));
        picker.key(&press(KeyCode::Char('d')));
        assert!(
            matches!(&picker.line, Some(PickerLine::Refused(why)) if why.contains("the default is in force"))
        );
        // A section says where it is changed.
        let mut picker = settings_picker(&list, Some("endpoints"));
        picker.key(&press(KeyCode::Enter));
        assert!(
            matches!(&picker.line, Some(PickerLine::Refused(why)) if why.contains("a section"))
        );
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
            wires: Vec::new(),
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
                wires: Vec::new(),
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
            classifier_model: Some("claude-haiku-5-5".into()),
        };
        assert_eq!(
            report_lines(&report),
            [
                "realm: /w/.rkat/tui (id tui)",
                "shell confinement: workspace",
                "permission mode: auto, PAUSED (consecutive denials 1/3, total 2/20)",
                "classifier: claude-haiku-5-5 (/auto stats shows its numbers)",
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
            ui: crate::config::Typed::defaults().unwrap().ui,
            compaction: crate::ui::CompactionPolicy::default(),
            pricing: crate::pricing::PricingSetup::default(),
            reroot: None,
            wire_files: None,
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
        runner.reports.epoch = 2;
        let other = SessionId::new();
        runner.switches.running = true;
        runner.on_report(1, switched(&other, "dead"));
        let link_now = runner.conn.link().unwrap();
        assert_eq!(link_now.stream, "live");
        assert_eq!(link_now.session, live);
        assert!(runner.switches.running, "a switch started since still runs");
        while let Ok(msg) = ui_rx.try_recv() {
            assert!(!matches!(msg, Msg::ResetTranscript(_)), "{msg:?}");
        }
        // The same report of the current link is shown.
        runner.on_report(2, switched(&other, "next"));
        let link_now = runner.conn.link().unwrap();
        assert_eq!(link_now.stream, "next");
        assert_eq!(link_now.session, other);
        assert!(!runner.switches.running);
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
        assert!(runner.turn.is_open(), "the unanswered prompt's turn runs");
        assert!(runner.turn.interrupted(), "Esc interrupted it at once");
        // The steer is expected at the next boundary while its send runs.
        assert!(runner.steering.steers.holds(PendingId(1)));
        // Nothing was answered: no turn end, no acknowledgement yet.
        while let Ok(msg) = ui_rx.try_recv() {
            assert!(
                !matches!(msg, Msg::TurnFinished(_) | Msg::SteerAck { .. }),
                "{msg:?}"
            );
        }
    }

    /// A steer typed while the prompt that starts its turn waits for the
    /// first link waits behind it, so it steers that turn instead of being
    /// refused for want of one (and held by the UI until that turn ends);
    /// with no prompt waiting it goes at once.
    #[tokio::test]
    async fn a_steer_waits_behind_a_held_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, _ui_rx) = runner(dir.path());
        let steer = Effect::Steer {
            id: PendingId(1),
            text: "and this".into(),
        };
        assert!(
            runner
                .unless_held(Effect::StartTurn("hello".into()))
                .is_none()
        );
        assert!(runner.unless_held(steer.clone()).is_none());
        assert_eq!(
            runner.unless_held(Effect::StatusReport),
            Some(Effect::StatusReport)
        );
        runner.conn = up(link("live", SessionId::new()).await);
        assert_eq!(
            runner.releasable(),
            vec![Effect::StartTurn("hello".into()), steer.clone()]
        );
        assert_eq!(runner.unless_held(steer.clone()), Some(steer));
    }

    /// A refused `/resume` ends the switch; nothing else that reports a
    /// notice does.
    #[tokio::test]
    async fn only_a_refused_switch_ends_the_switch() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut ui_rx) = runner(dir.path());
        runner.conn = up(link("live", SessionId::new()).await);
        runner.switches.running = true;
        runner.on_report(
            0,
            Report::Interrupted {
                epoch: None,
                result: Err("x".into()),
            },
        );
        assert!(runner.switches.running);
        // One of a lost connection says why, and leaves a later switch be.
        runner.reports.epoch = 1;
        runner.on_report(
            0,
            Report::SwitchRefused(NoticeLevel::Info, "already in this conversation".into()),
        );
        assert!(runner.switches.running);
        runner.on_report(
            1,
            Report::SwitchRefused(NoticeLevel::Info, "already in this conversation".into()),
        );
        assert!(!runner.switches.running);
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
        runner.turn = prompted(1);
        let early = InputId::new();
        for id in 1..=3 {
            runner.steering.steers.sending(PendingId(id));
        }
        // 2's input joined the run before its send answered.
        assert!(runner.steering.steers.applied(&early).is_empty());
        runner.sent(1, Some(PendingId(1)), Ok(engine(InputId::new())));
        runner.sent(1, Some(PendingId(2)), Ok(engine(early)));
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
        assert!(runner.steering.steers.holds(PendingId(1)));
        assert!(!runner.steering.steers.holds(PendingId(3)));
    }

    /// The daemon's stop gives back messages: one this client sent that
    /// the agent never read goes back to the editor with why; one it saw
    /// delivered that the engine did not keep comes back with its text;
    /// another client's comes back to nobody here.
    #[tokio::test]
    async fn the_stop_s_given_back_messages_return_to_their_client() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut ui_rx) = runner(dir.path());
        runner.turn = prompted(1);
        let (two, three, other) = (InputId::new(), InputId::new(), InputId::new());
        for (id, input) in [(2, &two), (3, &three)] {
            runner.steering.steers.sending(PendingId(id));
            runner.sent(
                1,
                Some(PendingId(id)),
                Ok(Some(remote::Admitted {
                    input: input.clone(),
                })),
            );
        }
        assert_eq!(runner.steering.steers.applied(&three), vec![PendingId(3)]);
        drain(&mut ui_rx);
        let agent = String::new();
        runner.given_back(protocol::GivenBack {
            agent: agent.clone(),
            ids: vec![two, other.clone()],
            now: protocol::GivenBackNow::NotRead { why: "gone".into() },
        });
        runner.given_back(protocol::GivenBack {
            agent: agent.clone(),
            ids: vec![three],
            now: protocol::GivenBackNow::Unkept {
                why: "cut".into(),
                text: "three".into(),
            },
        });
        runner.given_back(protocol::GivenBack {
            agent,
            ids: vec![other],
            now: protocol::GivenBackNow::Unkept {
                why: "cut".into(),
                text: "not mine".into(),
            },
        });
        let msgs = drain(&mut ui_rx);
        assert!(
            msgs.iter().any(|m| matches!(m, Msg::NotDelivered { id: PendingId(2), why: Some(why) } if why == "gone")),
            "{msgs:?}"
        );
        let unkept: Vec<&str> = msgs
            .iter()
            .filter_map(|m| match m {
                Msg::Unkept { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(unkept, ["three"], "{msgs:?}");
        assert!(runner.steering.steers.outstanding_inputs().is_empty());
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
                reasoning_effort: None,
            })
        }

        /// The start of run `run` whose input is `text`.
        fn started_with(&mut self, run: &RunId, text: &str) -> EventEnvelope<AgentEvent> {
            use meerkat_core::types::{ContentInput, RunInput, TranscriptMessageIdentity};
            self.envelope(AgentEvent::RunStarted {
                session_id: self.session.clone(),
                identity: TranscriptMessageIdentity {
                    run_id: Some(run.clone()),
                    ..TranscriptMessageIdentity::default()
                },
                input: RunInput::Content {
                    content: ContentInput::Text(text.into()),
                },
                request_reasoning: None,
                reasoning_effort: None,
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

        fn resumed(&mut self, run: &RunId) -> EventEnvelope<AgentEvent> {
            self.envelope(AgentEvent::RunResumed {
                session_id: self.session.clone(),
                run_id: run.clone(),
                round: 1,
                resume_point: meerkat_core::event::RunResumePointKind::ToolsPending,
                reasoning_effort: None,
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
        runner.turn = prompted(0);
        runner.on_event(feed.started(&first), None);
        runner.on_event(feed.completed(&first), None);
        assert!(runner.turn.is_open(), "the stale notice ends nothing");
        // Another client's steer runs as a follow-up run: still the turn.
        runner.on_event(feed.started(&follow_up), None);
        runner.idle_reported(feed.idle(&first, 0));
        assert!(runner.turn.is_open(), "a run is open");
        // The notice may come before the run's end shows here.
        runner.idle_reported(feed.idle(&follow_up, 0));
        assert!(runner.turn.is_open());
        assert_eq!(turn_ends(&drain(&mut rx)), 0);
        runner.on_event(feed.completed(&follow_up), None);
        assert!(!runner.turn.is_open());
        assert_eq!(turn_ends(&drain(&mut rx)), 1);
    }

    /// The owner's report: while the agent worked,
    /// his messages were held by the UI (↑ took them back, Esc sent them),
    /// never steered. The cause: the quiet turn of `/compact`'s run took
    /// every run that followed it before the agent went idle (a helper's
    /// note or peer message queued behind the compaction, or ahead of it),
    /// so the agent's work drew nothing and the UI held what was typed as
    /// "the compaction runs". A run that is not the compaction's ends the
    /// quiet turn: it opens the UI's turn (`RemoteTurn`, so typed messages
    /// are steered), its events reach the UI, and the UI is told the
    /// compaction's run is over, after the turn opened.
    #[tokio::test]
    async fn a_run_after_the_compaction_s_ends_the_quiet_turn() {
        use crate::ui::COMPACT_PROMPT;
        let dir = tempfile::tempdir().unwrap();
        // The helper's run after the compaction's, back to back (no idle
        // between), and the helper's run before it (queued ahead of the
        // compaction prompt).
        for compaction_first in [true, false] {
            let (mut runner, mut rx) = runner(dir.path());
            let mut feed = Feed {
                session: SessionId::new(),
                seq: 0,
            };
            let (compact, helper) = (RunId::new(), RunId::new());
            runner.turn = Turn::open(Kind::Compaction { id: 0 });
            if compaction_first {
                runner.on_event(feed.started_with(&compact, COMPACT_PROMPT), None);
                runner.on_event(feed.completed(&compact), None);
                assert!(runner.turn.is_quiet(), "{compaction_first}");
            }
            runner.on_event(
                feed.started_with(&helper, "Your helper `scout` finished its task."),
                None,
            );
            assert!(
                !runner.turn.is_quiet(),
                "{compaction_first}: the quiet turn ended"
            );
            assert!(
                runner.turn.is_open(),
                "{compaction_first}: the helper's run is a turn"
            );
            let msgs = drain(&mut rx);
            let opened = msgs
                .iter()
                .position(|m| matches!(m, Msg::RemoteTurn { .. }));
            let over = msgs
                .iter()
                .position(|m| matches!(m, Msg::CompactionRun(false)));
            assert!(
                opened.is_some() && over.is_some() && opened < over,
                "{compaction_first}: the turn opens, then the compaction's run is over: {msgs:?}"
            );
            assert!(
                msgs.iter().any(|m| matches!(m, Msg::Agent(env) if matches!(env.payload, AgentEvent::RunStarted { .. }))),
                "{compaction_first}: the run's events are shown: {msgs:?}"
            );
            // A message typed now is steered into the run, not refused.
            runner.steer(PendingId(7), "and this");
            let msgs = drain(&mut rx);
            assert!(
                !msgs.iter().any(|m| matches!(
                    m,
                    Msg::SteerAck {
                        result: Err(SteerFailure::NotRunning),
                        ..
                    }
                )),
                "{compaction_first}: {msgs:?}"
            );
            runner.on_event(feed.completed(&helper), None);
            runner.idle_reported(feed.idle(&helper, 0));
            assert!(!runner.turn.is_open(), "{compaction_first}");
            assert_eq!(turn_ends(&drain(&mut rx)), 1, "{compaction_first}");
        }
    }

    fn remote_turns(msgs: &[Msg]) -> Vec<Option<String>> {
        msgs.iter()
            .filter_map(|m| match m {
                Msg::RemoteTurn { prompt } => Some(prompt.clone()),
                _ => None,
            })
            .collect()
    }

    /// The daemon's note of an open run joins its turn and makes it the
    /// run to stop. A live start or resumption of the same run after it is
    /// no second turn; its end and the idle notice end the turn; a stale
    /// idle notice of an earlier run ends nothing; the same note again (a
    /// reconnect) and the run's end replayed after the turn do not join.
    #[tokio::test]
    async fn the_daemon_s_note_of_an_open_run_joins_its_turn_once() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut rx) = runner(dir.path());
        let mut feed = Feed {
            session: SessionId::new(),
            seq: 0,
        };
        let (earlier, run) = (RunId::new(), RunId::new());
        runner.idle_reported(feed.idle(&earlier, 0));
        runner.run_noted(run.clone());
        assert!(runner.turn.is_open());
        assert_eq!(open(&runner).runs.current.as_ref(), Some(&run));
        assert_eq!(remote_turns(&drain(&mut rx)), vec![None]);
        runner.settle_turn();
        assert!(runner.turn.is_open(), "the stale notice ends nothing");
        runner.on_event(feed.resumed(&run), None);
        runner.run_noted(run.clone());
        assert_eq!(open(&runner).runs.noted, None, "the stream shows it now");
        assert!(open(&runner).runs.open.contains(&run));
        assert!(remote_turns(&drain(&mut rx)).is_empty(), "one turn");
        runner.on_event(feed.completed(&run), None);
        runner.idle_reported(feed.idle(&run, 0));
        assert!(!runner.turn.is_open());
        assert_eq!(turn_ends(&drain(&mut rx)), 1);
        runner.run_noted(run.clone());
        runner.on_event(feed.completed(&run), None);
        assert!(!runner.turn.is_open(), "the run ended in the last turn");
        let msgs = drain(&mut rx);
        assert!(remote_turns(&msgs).is_empty(), "{msgs:?}");
    }

    /// A noted run whose end fell before this client's stream opened: the
    /// idle notice naming it ends the turn, and its end arriving after all
    /// joins no turn. A run that starts in between ends the noted one (an
    /// agent runs one run at a time): its start is shown as another
    /// client's, it is the run to stop, and its end ends the turn.
    #[tokio::test]
    async fn a_noted_run_ends_on_its_idle_notice_or_when_another_starts() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut rx) = runner(dir.path());
        let mut feed = Feed {
            session: SessionId::new(),
            seq: 0,
        };
        let run = RunId::new();
        runner.run_noted(run.clone());
        runner.idle_reported(feed.idle(&run, 0));
        assert!(!runner.turn.is_open(), "its end did not show here");
        runner.on_event(feed.completed(&run), None);
        assert!(!runner.turn.is_open());
        let msgs = drain(&mut rx);
        assert_eq!(turn_ends(&msgs), 1, "{msgs:?}");
        assert_eq!(remote_turns(&msgs), vec![None], "{msgs:?}");

        let (noted, next) = (RunId::new(), RunId::new());
        runner.run_noted(noted.clone());
        runner.on_event(feed.started(&next), None);
        assert_eq!(open(&runner).runs.current.as_ref(), Some(&next));
        assert_eq!(open(&runner).runs.noted, None);
        assert!(open(&runner).runs.ended.contains(&noted));
        assert_eq!(
            remote_turns(&drain(&mut rx)),
            vec![None, Some("x".to_string())],
            "the new run's prompt shows"
        );
        runner.idle_reported(feed.idle(&noted, 0));
        assert!(runner.turn.is_open(), "the next run is open");
        runner.on_event(feed.completed(&next), None);
        runner.idle_reported(feed.idle(&next, 0));
        assert!(!runner.turn.is_open());
        assert_eq!(turn_ends(&drain(&mut rx)), 1);
    }

    /// `/model` moves the model a helper is shown on until its own is read:
    /// a helper that names no model runs on the agent's model as it is now,
    /// not as it was at the attach.
    #[tokio::test]
    async fn a_model_switch_moves_the_helpers_shown_model() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, _rx) = runner(dir.path());
        let session = SessionId::new();
        runner.conn = up(link("live", session.clone()).await);
        runner.helpers.followers.set_default_model("at-attach");
        let switch = ModelSwitch {
            model: "now".into(),
            provider: Some("anthropic".into()),
            auth_binding: None,
            endpoint: "anthropic".into(),
            listed: None,
        };
        runner.model_set(
            switch,
            Ok(protocol::ModelSet {
                session_id: session,
                model: "now".into(),
                provider: "anthropic".into(),
                reseated: false,
            }),
        );
        assert_eq!(runner.helpers.followers.default_model(), "now");
    }

    /// In this client's own turn, a note before the stream's start of the
    /// run makes it the run to stop and joins no second turn; the prompt is
    /// not shown again when the start arrives.
    #[tokio::test]
    async fn a_note_in_this_client_s_own_turn_only_names_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut rx) = runner(dir.path());
        let mut feed = Feed {
            session: SessionId::new(),
            seq: 0,
        };
        runner.turn = prompted(1);
        let run = RunId::new();
        runner.run_noted(run.clone());
        assert_eq!(open(&runner).runs.current.as_ref(), Some(&run));
        runner.on_event(feed.started(&run), None);
        assert!(open(&runner).runs.open.contains(&run));
        assert_eq!(open(&runner).runs.noted, None);
        assert!(remote_turns(&drain(&mut rx)).is_empty());
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
            runner.turn = prompted(0);
            runner.on_event(feed.started(&run), None);
            runner.steering.steers.sending(PendingId(7));
            // Its own run's content is its text (`started` runs "x").
            runner.steering.steers.sent_text(PendingId(7), "x");
            runner.on_event(feed.completed(&run), None);
            runner.idle_reported(feed.idle(&run, 0));
            assert!(runner.turn.is_open(), "its send is unanswered");
            runner.sent(0, Some(PendingId(7)), Ok(engine(input.clone())));
            // The daemon decided before the steer arrived; it holds it now.
            assert!(!runner.turn.is_open(), "nothing on its way any more");
            let msgs = drain(&mut rx);
            assert_eq!(turn_ends(&msgs), 1);
            assert!(
                !msgs.iter().any(|m| matches!(m, Msg::SteerDelivered(_))),
                "not read yet: {msgs:?}"
            );
            assert!(runner.steering.steers.holds(PendingId(7)));
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
            assert!(!runner.steering.steers.holds(PendingId(7)));
            runner.finish_turn();
            drain(&mut rx);
        }
    }

    /// A later run of the turn started by a daemon's note,
    /// while this client's steer is still queued behind it, does not mark
    /// the steer delivered (before, every steer was taken as carried by
    /// whatever run came next); the steer is marked when its own run
    /// starts.
    #[tokio::test]
    async fn a_run_of_a_note_does_not_deliver_a_queued_steer() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut rx) = runner(dir.path());
        let mut feed = Feed {
            session: SessionId::new(),
            seq: 0,
        };
        let (first, note, own) = (RunId::new(), RunId::new(), RunId::new());
        runner.turn = prompted(0);
        runner.on_event(feed.started(&first), None);
        runner.steering.steers.sending(PendingId(7));
        runner
            .steering
            .steers
            .sent_text(PendingId(7), "also update the docs");
        runner.sent(0, Some(PendingId(7)), Ok(engine(InputId::new())));
        runner.on_event(feed.completed(&first), None);
        drain(&mut rx);
        runner.on_event(
            feed.started_with(&note, "rkat-tui note: helper `scout` finished"),
            None,
        );
        let msgs = drain(&mut rx);
        assert!(
            !msgs.iter().any(|m| matches!(m, Msg::SteerDelivered(_))),
            "{msgs:?}"
        );
        assert!(runner.steering.steers.holds(PendingId(7)));
        runner.on_event(feed.completed(&note), None);
        runner.on_event(feed.started_with(&own, "also update the docs"), None);
        let msgs = drain(&mut rx);
        assert_eq!(
            msgs.iter()
                .filter(|m| matches!(m, Msg::SteerDelivered(PendingId(7))))
                .count(),
            1,
            "{msgs:?}"
        );
        assert!(!runner.steering.steers.holds(PendingId(7)));
    }

    /// A prompt sent as a helper's message started a run: that run's
    /// start, which does not carry the prompt, is shown as what joined the
    /// turn (the message), and the run that carries the prompt shows
    /// nothing of its own (the UI showed it).
    #[tokio::test]
    async fn a_run_that_does_not_carry_the_prompt_is_not_taken_for_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut rx) = runner(dir.path());
        let mut feed = Feed {
            session: SessionId::new(),
            seq: 0,
        };
        let (note, own) = (RunId::new(), RunId::new());
        runner.turn = Turn::prompt(1, "fix the build".into());
        runner.on_event(
            feed.started_with(&note, "rkat-tui note: helper `scout` finished"),
            None,
        );
        assert_eq!(
            remote_turns(&drain(&mut rx)),
            vec![Some("rkat-tui note: helper `scout` finished".to_string())]
        );
        runner.on_event(feed.completed(&note), None);
        runner.on_event(feed.started_with(&own, "fix the build"), None);
        assert!(remote_turns(&drain(&mut rx)).is_empty());
        assert!(runner.turn.is_open());
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
            runner.steering.steers.sending(PendingId(n));
            runner.sent(0, Some(PendingId(n)), Ok(engine(input.clone())));
        }
        drain(&mut rx);
        let run = RunId::new();
        runner.idle_reported(feed.idle(&run, 1));
        assert_eq!(
            runner.steering.fates,
            FatesAsk::NotAsked,
            "the runtime still holds one"
        );
        runner.idle_reported(feed.idle(&run, 0));
        assert_eq!(runner.steering.fates, FatesAsk::Asking);
        runner.idle_reported(feed.idle(&run, 0));
        assert_eq!(
            runner.steering.fates,
            FatesAsk::AskAgain,
            "asked again on the answer"
        );
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
        assert!(!runner.steering.steers.holds(PendingId(1)));
        assert!(!runner.steering.steers.holds(PendingId(2)));
        assert!(
            runner.steering.steers.holds(PendingId(3))
                && runner.steering.steers.holds(PendingId(4))
        );
        assert!(
            !runner.steering.admitted.contains(&PendingId(2)),
            "not saved as sent"
        );
        assert!(runner.steering.admitted.contains(&PendingId(1)));
        // The notice that came meanwhile asked again, for the two left.
        assert_eq!(runner.steering.fates, FatesAsk::Asking);
        // A failed question leaves them followed; nothing is asked again
        // until the next notice.
        runner.input_fates(Err("lost".into()));
        assert_eq!(runner.steering.fates, FatesAsk::NotAsked);
        assert!(drain(&mut rx).is_empty());
        assert!(runner.steering.steers.holds(PendingId(3)));
        // The answer to a question for a steer settled since marks nothing.
        runner.input_fates(Ok(protocol::InputFates {
            inputs: vec![row(0, protocol::InputFate::Taken)],
        }));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_turn_keeps_its_worst_end() {
        let mut turn = prompted(1);
        turn.record(Ok(TurnSummary::default()));
        turn.record(Err(TurnFailure::Interrupted));
        turn.record(Ok(TurnSummary::default()));
        let result = |turn: &Turn| turn.open_ref().unwrap().result.clone();
        assert_eq!(result(&turn), Some(Err(TurnFailure::Interrupted)));
        turn.record(Err(TurnFailure::Failed("x".into())));
        turn.record(Err(TurnFailure::Interrupted));
        assert_eq!(result(&turn), Some(Err(TurnFailure::Failed("x".into()))));
        // With no turn open there is nothing to record into.
        let mut idle = Turn::Idle;
        idle.record(Err(TurnFailure::Interrupted));
        assert!(idle.open_ref().is_none());
    }

    /// An interrupt the daemon does not confirm in time is followed by a
    /// hard cancel of the agent's current run and a second wait; only
    /// then does the turn end, as unconfirmed (never called interrupted).
    #[tokio::test]
    async fn an_unconfirmed_interrupt_escalates_once_then_ends_unconfirmed() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut ui_rx) = runner(dir.path());
        runner.conn = up(link("live", SessionId::new()).await);
        runner.turn = prompted(0);
        open_mut(&mut runner).interrupt = Some(interrupt_at(Instant::now(), 0, None));
        runner.on_deadline();
        assert!(runner.turn.is_open(), "it waits once more");
        let deadline = open(&runner).interrupt.as_ref().unwrap().deadline;
        assert!(
            matches!(deadline, Deadline::Escalated(at) if at > Instant::now()),
            "{deadline:?}"
        );
        let notices: Vec<String> = std::iter::from_fn(|| ui_rx.try_recv().ok())
            .filter_map(|msg| match msg {
                Msg::Notice { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert!(
            notices
                .iter()
                .any(|t| t
                    .contains("the interrupted run did not end within 10s; cancelling it at once")),
            "{notices:?}"
        );
        open_mut(&mut runner).interrupt.as_mut().unwrap().deadline =
            Deadline::Escalated(Instant::now());
        runner.on_deadline();
        assert!(!runner.turn.is_open());
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

    /// An interrupt numbered `epoch`, first pressed `now`, for `run`, due
    /// at once.
    fn interrupt_at(now: Instant, epoch: u64, run: Option<RunId>) -> Interrupt {
        Interrupt {
            epoch,
            first: now,
            last_press: now,
            run,
            forced: false,
            said_stopped: false,
            deadline: Deadline::Waiting(now),
        }
    }

    use super::turn::ALREADY_STOPPED;

    /// A link like [`link`] whose daemon side hands over every request
    /// line after the attach (none are answered).
    async fn link_recording(
        stream: &str,
        session: SessionId,
    ) -> (Link, mpsc::UnboundedReceiver<serde_json::Value>) {
        use tokio::io::AsyncBufReadExt as _;
        let (client, server) = tokio::io::duplex(1 << 16);
        let (server_read, mut server_write) = tokio::io::split(server);
        let (tx, rx) = mpsc::unbounded_channel();
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
            let mut line = String::new();
            while lines.read_line(&mut line).await.is_ok_and(|n| n > 0) {
                if let Ok(request) = serde_json::from_str(&line) {
                    let _ = tx.send(request);
                }
                line.clear();
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
        let link = Link {
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
        };
        (link, rx)
    }

    /// The interrupt requests sent so far (`rkat/interrupt`'s params),
    /// after the spawned sends had their turn.
    async fn interrupts_sent(
        rx: &mut mpsc::UnboundedReceiver<serde_json::Value>,
    ) -> Vec<serde_json::Value> {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        let mut sent = Vec::new();
        while let Ok(request) = rx.try_recv() {
            assert_ne!(request["method"], "mob/stop_member_run", "{request}");
            assert_ne!(request["method"], "mob/force_cancel", "{request}");
            assert_ne!(request["method"], "mob/hard_cancel_member", "{request}");
            if request["method"] == "rkat/interrupt" {
                sent.push(request["params"].clone());
            }
        }
        sent
    }

    fn notices(rx: &mut mpsc::UnboundedReceiver<Msg>) -> Vec<String> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|msg| match msg {
                Msg::Notice { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }

    /// The owner's report: the first press stopped the run,
    /// a typed message started a follow-up run, and the next presses (and
    /// the hard cancel at the deadline) hit that run. Now, once the
    /// interrupted run has ended, presses in quick succession send nothing
    /// and say once that the turn is already stopped, the deadline cancels
    /// nothing, and the same after the turn's end.
    #[tokio::test]
    async fn presses_after_the_interrupted_run_ended_touch_no_later_run() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut ui_rx) = runner(dir.path());
        let (link, mut requests) = link_recording("live", SessionId::new()).await;
        runner.conn = up(link);
        let (first, later) = (RunId::new(), RunId::new());
        let now = Instant::now();
        runner.turn = Turn::open(Kind::Joined);
        open_mut(&mut runner).interrupt = Some(interrupt_at(now, 1, Some(first.clone())));
        open_mut(&mut runner).runs.seen(&first);
        open_mut(&mut runner).runs.over(&first);
        // A follow-up run (another client's message) is open now.
        open_mut(&mut runner).runs.seen(&later);
        for _ in 0..3 {
            runner.interrupt();
        }
        runner.on_deadline();
        assert!(runner.turn.is_open(), "the turn ends as its runs do");
        assert!(interrupts_sent(&mut requests).await.is_empty());
        let said = notices(&mut ui_rx);
        assert_eq!(
            said.iter().filter(|text| *text == ALREADY_STOPPED).count(),
            1,
            "{said:?}"
        );
        assert!(
            !said.iter().any(|text| text.contains("cancelling")),
            "{said:?}"
        );

        // After the turn's end, a press right away acts on nothing either,
        // and says nothing again.
        open_mut(&mut runner).runs.over(&later);
        runner.finish_turn();
        runner.interrupt();
        assert!(interrupts_sent(&mut requests).await.is_empty());
        assert!(
            !notices(&mut ui_rx)
                .iter()
                .any(|text| text == ALREADY_STOPPED)
        );
    }

    /// The first press stops the run by its id, with the messages the
    /// agent has not read; a press after [`STOP_GRACE`] while that run has
    /// not ended forces that run (and names no other).
    #[tokio::test]
    async fn the_first_press_stops_its_run_and_a_later_one_forces_that_run() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, _ui_rx) = runner(dir.path());
        let (link, mut requests) = link_recording("live", SessionId::new()).await;
        runner.conn = up(link);
        let run = RunId::new();
        runner.turn = Turn::open(Kind::Joined);
        open_mut(&mut runner).runs.seen(&run);
        runner.steering.steers.sending(PendingId(1));
        let input = InputId::new();
        runner.steering.steers.admitted(PendingId(1), input.clone());
        runner.interrupt();
        let sent = interrupts_sent(&mut requests).await;
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0]["target"], "run");
        assert_eq!(sent[0]["run_id"], serde_json::to_value(&run).unwrap());
        assert_eq!(sent[0]["hard"], false);
        assert_eq!(
            sent[0]["input_ids"],
            serde_json::json!([serde_json::to_value(&input).unwrap()])
        );
        // Past the grace, the run still open, and another run noted meanwhile
        // as current: the force names the interrupted run.
        let past = Instant::now().checked_sub(STOP_GRACE * 2).unwrap();
        let interrupt = open_mut(&mut runner).interrupt.as_mut().unwrap();
        interrupt.first = past;
        interrupt.last_press = past;
        open_mut(&mut runner).runs.current = Some(RunId::new());
        runner.interrupt();
        let forced = interrupts_sent(&mut requests).await;
        assert_eq!(forced.len(), 1, "{forced:?}");
        assert_eq!(forced[0]["run_id"], serde_json::to_value(&run).unwrap());
        assert_eq!(forced[0]["hard"], true);
        // The run goes on past the deadline: it is cancelled at once, by
        // its id.
        open_mut(&mut runner).interrupt.as_mut().unwrap().deadline =
            Deadline::Waiting(Instant::now());
        runner.on_deadline();
        let hard = interrupts_sent(&mut requests).await;
        assert_eq!(hard.len(), 1, "{hard:?}");
        assert_eq!(hard[0]["run_id"], serde_json::to_value(&run).unwrap());
        assert_eq!(hard[0]["hard"], true);
    }

    /// A link whose daemon side takes every `mob/member_send` and never
    /// answers it (the answer is lost on the way: the daemon admitted the
    /// input), counting them and checking each carries the client's key,
    /// and answers `rkat/sent` with `fate` (`None`: no answer either).
    async fn link_losing_answers(
        fate: Option<protocol::SentFate>,
    ) -> (Link, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
        let (client, server) = tokio::io::duplex(1 << 16);
        let (server_read, mut server_write) = tokio::io::split(server);
        let sends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&sends);
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
            let mut line = String::new();
            while lines.read_line(&mut line).await.is_ok_and(|n| n > 0) {
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                line.clear();
                if request["method"] == "mob/member_send" {
                    assert!(
                        request["params"][protocol::SEND_KEY].is_string(),
                        "{request}"
                    );
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                } else if request["method"] == "rkat/sent"
                    && let Some(fate) = &fate
                {
                    let answer = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": fate,
                    });
                    server_write
                        .write_all(format!("{answer}\n").as_bytes())
                        .await
                        .unwrap();
                }
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
        let link = Link {
            connection: Arc::new(connection),
            notifications,
            stream: "live".to_string(),
            session: SessionId::new(),
            endpoint: Endpoint {
                address,
                token: "t".into(),
                pid: 1,
                workspace: None,
                started_at: None,
            },
        };
        (link, sends)
    }

    /// A steer whose answer is lost after the daemon
    /// admitted it is not told as refused. The client asks the daemon by
    /// the send's key: admitted, it is acknowledged and followed as any
    /// steer (nothing returns to the editor, so nothing is sent twice);
    /// not received, it is refused (back in the editor: the agent does not
    /// have it); with no word from the daemon its fate is unknown (not
    /// back in the editor either). Each case reached the daemon once.
    #[tokio::test(start_paused = true)]
    async fn a_send_whose_answer_was_lost_asks_the_daemon_before_it_is_told() {
        let input = InputId::new();
        for (fate, expect) in [
            (
                Some(protocol::SentFate::Admitted {
                    input_id: Some(input.clone()),
                }),
                "admitted",
            ),
            (Some(protocol::SentFate::NotReceived), "refused"),
            (None, "unknown"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (mut runner, mut ui_rx) = runner(dir.path());
            let (link, sends) = link_losing_answers(fate).await;
            runner.conn = up(link);
            runner.turn = prompted(1);
            runner.steer(PendingId(1), "and this");
            let report = loop {
                let (_, report) = runner.reports.rx.recv().await.unwrap();
                if let Report::Sent { result, .. } = report {
                    break result;
                }
            };
            assert_eq!(
                sends.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "{expect}"
            );
            match expect {
                "admitted" => assert_eq!(
                    report,
                    Ok(Some(remote::Admitted {
                        input: input.clone(),
                    }))
                ),
                "refused" => assert!(matches!(report, Err(SendFailure::NotSent(_))), "{report:?}"),
                _ => assert!(matches!(report, Err(SendFailure::Unknown(_))), "{report:?}"),
            }
            runner.sent(1, Some(PendingId(1)), report);
            let acks: Vec<Result<(), SteerFailure>> = drain(&mut ui_rx)
                .into_iter()
                .filter_map(|m| match m {
                    Msg::SteerAck { result, .. } => Some(result),
                    _ => None,
                })
                .collect();
            match expect {
                "admitted" => {
                    assert_eq!(acks, [Ok(())]);
                    assert!(
                        runner.steering.steers.holds(PendingId(1)),
                        "followed by its input"
                    );
                }
                "refused" => assert!(matches!(acks.as_slice(), [Err(SteerFailure::Rejected(_))])),
                _ => assert!(matches!(acks.as_slice(), [Err(SteerFailure::Unknown(_))])),
            }
        }
    }

    /// The same for a prompt: admitted, its turn goes on; unknown, the
    /// turn ends saying it may have reached the agent.
    #[tokio::test(start_paused = true)]
    async fn a_prompt_whose_answer_was_lost_is_not_told_as_unsent() {
        for fate in [
            Some(protocol::SentFate::Admitted { input_id: None }),
            Some(protocol::SentFate::OnItsWay),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (mut runner, mut ui_rx) = runner(dir.path());
            let admitted = matches!(fate, Some(protocol::SentFate::Admitted { .. }));
            let (link, sends) = link_losing_answers(fate).await;
            runner.conn = up(link);
            runner.start_turn("hello");
            let report = loop {
                let (_, report) = runner.reports.rx.recv().await.unwrap();
                if let Report::Sent { result, .. } = report {
                    break result;
                }
            };
            assert_eq!(sends.load(std::sync::atomic::Ordering::SeqCst), 1);
            let id = runner.turn.id().unwrap();
            runner.sent(id, None, report);
            let msgs = drain(&mut ui_rx);
            if admitted {
                assert!(runner.turn.is_open(), "its run shows when it reports");
                assert_eq!(turn_ends(&msgs), 0);
            } else {
                assert!(!runner.turn.is_open());
                assert!(
                    msgs.iter().any(|m| matches!(
                        m,
                        Msg::TurnFinished(Err(TurnFailure::Failed(why))) if why.starts_with(turn::UNKNOWN_FATE)
                    )),
                    "{msgs:?}"
                );
            }
        }
    }

    /// Esc while the daemon is slow to answer; the run ends
    /// by itself; after a pause Esc again opens a new interrupt of the
    /// turn. The first call's late answer (or its timeout) still counts as
    /// this turn's: once both are answered, in either order, the turn ends
    /// on the idle notice. Before, the first answer was "not current" and
    /// the turn stayed running for good.
    #[tokio::test]
    async fn a_turn_ends_after_two_interrupts_answered_in_either_order() {
        let dir = tempfile::tempdir().unwrap();
        for first_answered_first in [true, false] {
            let (mut runner, mut ui_rx) = runner(dir.path());
            let (link, mut requests) = link_recording("live", SessionId::new()).await;
            runner.conn = up(link);
            let mut feed = Feed {
                session: SessionId::new(),
                seq: 0,
            };
            let run = RunId::new();
            runner.turn = prompted(1);
            runner.on_event(feed.started(&run), None);
            runner.interrupt();
            let epoch = |runner: &Runner| open(runner).interrupt.as_ref().unwrap().epoch;
            let first = epoch(&runner);
            runner.on_event(feed.completed(&run), None);
            runner.idle_reported(feed.idle(&run, 0));
            assert!(runner.turn.is_open(), "the first interrupt is unanswered");
            // A pause, then Esc: a new interrupt of the turn.
            open_mut(&mut runner).interrupt.as_mut().unwrap().last_press =
                Instant::now().checked_sub(INTERRUPT_REPEAT * 2).unwrap();
            runner.interrupt();
            let second = epoch(&runner);
            assert_ne!(first, second, "{first_answered_first}");
            assert_eq!(interrupts_sent(&mut requests).await.len(), 2);
            let answer = || {
                Ok(protocol::Interrupted {
                    run_id: Some(run.clone()),
                    run: protocol::RunInterrupt::Ended,
                    follow_ups: 0,
                    returned: Vec::new(),
                    read: Vec::new(),
                })
            };
            let order = if first_answered_first {
                [first, second]
            } else {
                [second, first]
            };
            runner.interrupt_answered(Some(order[0]), answer());
            assert!(runner.turn.is_open(), "{first_answered_first}: one is left");
            // The first call timed out, the second was answered: either
            // way it is no longer awaited.
            let last = if order[1] == first {
                Err("the daemon did not answer in time".to_string())
            } else {
                answer()
            };
            runner.interrupt_answered(Some(order[1]), last);
            assert!(
                !runner.turn.is_open(),
                "{first_answered_first}: the turn ended"
            );
            let msgs = drain(&mut ui_rx);
            assert_eq!(turn_ends(&msgs), 1, "{first_answered_first}: {msgs:?}");
        }
    }

    /// The daemon's answer returns the messages it took back (to the
    /// editor), marks the read ones delivered, and only then lets the turn
    /// end.
    #[tokio::test]
    async fn the_interrupts_answer_returns_the_unread_messages_before_the_turn_ends() {
        let dir = tempfile::tempdir().unwrap();
        let (mut runner, mut ui_rx) = runner(dir.path());
        let run = RunId::new();
        runner.turn = Turn::open(Kind::Joined);
        let turn = open_mut(&mut runner);
        turn.interrupt = Some(Interrupt {
            deadline: Deadline::Off,
            ..interrupt_at(Instant::now(), 4, Some(run.clone()))
        });
        turn.unanswered_interrupts = vec![4];
        turn.result = Some(Err(TurnFailure::Interrupted));
        turn.runs.seen(&run);
        turn.runs.over(&run);
        let (unread, read) = (InputId::new(), InputId::new());
        runner.steering.steers.sending(PendingId(1));
        runner.steering.steers.admitted(PendingId(1), read.clone());
        runner.steering.steers.sending(PendingId(2));
        runner
            .steering
            .steers
            .admitted(PendingId(2), unread.clone());
        runner.turns.idle = Some(protocol::IdleNote {
            agent: "main".into(),
            session_id: SessionId::new(),
            last_run: Some(run.clone()),
            held: 0,
        });
        runner.settle_turn();
        assert!(runner.turn.is_open(), "it waits for the interrupt's answer");
        runner.interrupt_answered(
            Some(4),
            Ok(protocol::Interrupted {
                run_id: Some(run),
                run: protocol::RunInterrupt::Stopped,
                follow_ups: 0,
                returned: vec![unread],
                read: vec![read],
            }),
        );
        assert!(!runner.turn.is_open());
        let msgs: Vec<Msg> = std::iter::from_fn(|| ui_rx.try_recv().ok()).collect();
        let at = |pred: &dyn Fn(&Msg) -> bool| msgs.iter().position(pred);
        let returned = at(&|m| matches!(m, Msg::Returned(ids) if ids == &[PendingId(2)])).unwrap();
        let delivered = at(&|m| matches!(m, Msg::SteerDelivered(PendingId(1)))).unwrap();
        let ended = at(&|m| matches!(m, Msg::TurnFinished(Err(TurnFailure::Interrupted)))).unwrap();
        assert!(returned < ended && delivered < ended, "{msgs:?}");
    }
}
