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
use super::catchup::{Cursor, CursorFile, Mark};
use super::launch::{self, Decision};
use super::remote;
use super::transport::{ATTACH_TIMEOUT, Connection, Notification};
use crate::bootstrap::RuntimeScope;
use crate::cli::AttachMode;
use crate::daemon::endpoint::Endpoint;
use crate::delivery::{PendingId, SteerFailure};
use crate::endpoints::WireDialect;
use crate::ui::{
    CompactFailure, Effect, Msg, NoticeLevel, ShellOutcome, StatusUpdate, TurnFailure, TurnOutcome,
    TurnSummary,
};

/// The status-bar segment while the first connection is made.
pub const CONNECTING: &str = "connecting";
/// The status-bar segment while a started daemon comes up.
pub const STARTING: &str = "daemon starting";
/// The status-bar segment while a dropped connection is re-made.
pub const RECONNECTING: &str = "reconnecting";

/// What attach mode answers for what the daemon cannot do yet.
pub const NOT_AVAILABLE: &str = "not available in attach mode yet";

/// The notice shown on every attach.
pub const EXPERIMENTAL_NOTICE: &str = "attach mode is experimental: the agent runs in this workspace's rkat-tui-mob daemon, and its own shell and file tools run WITHOUT the approval gate for now (no review, no prompts). /quit detaches and leaves the daemon running; /daemon stop shuts it down.";

/// How long a turn whose runs all ended waits for a follow-up run.
pub const RUN_GRACE: Duration = Duration::from_millis(300);

/// How long after the first interrupt a second one force-cancels.
pub const STOP_GRACE: Duration = Duration::from_secs(1);

/// How long an interrupted turn waits for the daemon to report its end
/// before the UI is given it back.
pub const INTERRUPT_DEADLINE: Duration = Duration::from_secs(10);

/// How often the durable events are read again while a turn runs and the
/// live stream is quiet, and once after every attach: the replay
/// projection can lag the live stream, so events published just before
/// the stream opened may be in neither yet.
pub const CATCH_UP_EVERY: Duration = Duration::from_millis(500);

/// Bound on the calls that answer at once (sends, stops).
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

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
#[derive(Debug, Clone)]
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
}

impl AttachConfig {
    /// The attach mode the config stands for.
    #[must_use]
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
    /// The mark the replay started from.
    mark: Mark,
    replay: Vec<EventEnvelope<AgentEvent>>,
    identity: Option<(String, String)>,
}

/// What a background task reports.
enum Report {
    /// A `mob/turn_start` (the model switch) failed.
    TurnStart(ClientError),
    /// A stop or a cancel failed.
    Interrupt(String),
    /// A catch-up read (on the session it was read for).
    Replayed(SessionId, Vec<EventEnvelope<AgentEvent>>),
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

/// The attach-mode runner.
pub struct AttachRunner {
    config: AttachConfig,
    ui_tx: mpsc::UnboundedSender<Msg>,
    effect_rx: mpsc::UnboundedReceiver<Effect>,
    link: Option<Link>,
    connecting: Option<JoinHandle<Result<Established, ClientError>>>,
    /// The daemon may still be started (only the first connect does).
    first_connect: bool,
    reports_tx: mpsc::UnboundedSender<Report>,
    reports_rx: mpsc::UnboundedReceiver<Report>,
    mark: Option<Mark>,
    turn: Turn,
    /// `/model`: the model (and provider) the next prompt switches to.
    pending_model: Option<(String, Option<String>)>,
    /// `(provider, model)` as last read or switched to.
    model: Option<(String, String)>,
    /// `/daemon stop` was sent: a lost connection is not re-made.
    stopping: bool,
    /// When the durable events are read again ([`CATCH_UP_EVERY`]).
    catch_up_at: Option<Instant>,
    /// A catch-up read is running.
    catching_up: bool,
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
        Self {
            config,
            ui_tx,
            effect_rx,
            link: None,
            connecting: None,
            first_connect: true,
            reports_tx,
            reports_rx,
            mark: None,
            turn: Turn::default(),
            pending_model: None,
            model: None,
            stopping: false,
            catch_up_at: None,
            catching_up: false,
        }
    }

    /// Serve effects until the UI quits, then detach (the daemon and any
    /// running turn go on).
    pub async fn run(mut self) {
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
                Some(report) = self.reports_rx.recv() => self.on_report(report),
                () = sleep_until(self.turn.deadline()) => self.on_deadline(),
                () = sleep_until(self.catch_up_at) => self.catch_up(),
            }
        }
        self.save_cursor();
        if let Some(task) = self.connecting.take() {
            task.abort();
        }
    }

    fn send(&self, msg: Msg) {
        let _ = self.ui_tx.send(msg);
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
            .map(Mark::cursor)
            .or_else(|| self.config.cursor.load());
        let ui_tx = self.ui_tx.clone();
        let task = if std::mem::take(&mut self.first_connect) {
            let spawn = self.config.spawn.clone();
            tokio::spawn(async move {
                let endpoint = first_endpoint(&file, spawn.as_ref(), &ui_tx).await?;
                establish(endpoint, saved.as_ref()).await
            })
        } else {
            tokio::spawn(async move {
                let mut wait = BACKOFF_FIRST;
                loop {
                    if let Ok(endpoint) = Endpoint::read(&file) {
                        match establish(endpoint, saved.as_ref()).await {
                            Ok(established) => return Ok(established),
                            Err(err) => tracing::debug!(error = %err, "reconnect failed"),
                        }
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
            replay,
            identity,
        } = established;
        let address = link.endpoint.address;
        let session = link.session.to_string();
        let fresh = self.mark.is_none();
        if self
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
        if let Some((model, provider)) = identity
            && self.pending_model.is_none()
        {
            self.set_model_status(&provider, &model);
        }
        self.status(&format!("attached {address}"));
        self.link = Some(link);
        for envelope in replay {
            self.on_event(envelope);
        }
        self.catch_up_at = Some(Instant::now() + CATCH_UP_EVERY);
    }

    /// Read the durable events after the mark again (see
    /// [`CATCH_UP_EVERY`]); again later while a turn runs.
    fn catch_up(&mut self) {
        self.catch_up_at = None;
        let (Some(link), Some(mark)) = (&self.link, &self.mark) else {
            return;
        };
        if self.catching_up {
            return;
        }
        self.catching_up = true;
        let connection = Arc::clone(&link.connection);
        let session = link.session.clone();
        let after = mark.sequence();
        let reports = self.reports_tx.clone();
        tokio::spawn(async move {
            let events = match tokio::time::timeout(
                CALL_TIMEOUT,
                remote::replay(&connection, &session, after),
            )
            .await
            {
                Ok(Ok((events, _))) => events,
                _ => Vec::new(),
            };
            let _ = reports.send(Report::Replayed(session, events));
        });
    }

    /// The connection is gone: re-make it (unless the daemon was told to
    /// stop). A running turn goes on in the daemon; the catch-up shows it.
    fn lost(&mut self) {
        self.link = None;
        self.save_cursor();
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
        if let Some(envelope) = remote::stream_event(note, &link.stream) {
            self.on_event(envelope);
        }
    }

    fn set_model_status(&mut self, provider: &str, model: &str) {
        self.model = Some((provider.to_string(), model.to_string()));
        self.send(Msg::Status(StatusUpdate::Model {
            provider: provider.to_string(),
            model: model.to_string(),
        }));
        let window = self.config.config.as_ref().and_then(|config| {
            let provider = WireDialect::parse(provider)?.into();
            crate::endpoints::registry_context_window(config, provider, model)
        });
        self.send(Msg::Status(StatusUpdate::ContextWindow(window)));
    }

    fn save_cursor(&self) {
        if let Some(mark) = &self.mark
            && mark.sequence() > 0
        {
            self.config.cursor.save(&mark.cursor());
        }
    }

    /// One of the member's events (live or replayed): shown once
    /// ([`Mark`]), and what it means for the turn.
    fn on_event(&mut self, envelope: EventEnvelope<AgentEvent>) {
        if let Some(mark) = &mut self.mark
            && !mark.admit(envelope.seq)
        {
            return;
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
        if self.turn.active && self.link.is_some() {
            self.catch_up_at = Some(Instant::now() + CATCH_UP_EVERY);
        }
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
    }

    fn on_report(&mut self, report: Report) {
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
            Report::Replayed(session, events) => {
                self.catching_up = false;
                if self
                    .link
                    .as_ref()
                    .is_some_and(|link| link.session == session)
                {
                    for envelope in events {
                        self.on_event(envelope);
                    }
                }
                if self.turn.active && self.link.is_some() && self.catch_up_at.is_none() {
                    self.catch_up_at = Some(Instant::now() + CATCH_UP_EVERY);
                }
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
            Effect::RunShell { id, .. } => self.send(Msg::ShellFinished {
                id,
                outcome: ShellOutcome::Failed(format!("! commands are {NOT_AVAILABLE}")),
            }),
            Effect::SetModel(reference) => self.set_model(&reference),
            Effect::CompactNow => self.send(Msg::CompactRebuilt(Err(CompactFailure::Rebuild(
                NOT_AVAILABLE.to_string(),
            )))),
            Effect::StatusReport => self.status_report(),
            Effect::ContextReport(header) => self.notice(
                NoticeLevel::Info,
                format!("{header}\n(what the context holds is {NOT_AVAILABLE})"),
            ),
            Effect::Export { path, text } => self.export(path.as_deref(), &text),
            Effect::DaemonStop => self.daemon_stop().await,
            Effect::NewSession => self.unavailable("/new"),
            Effect::ResumeSession(_) | Effect::RequestResumePicker => self.unavailable("/resume"),
            Effect::RequestRewindMenu | Effect::Rewind(_) => self.unavailable("/rewind"),
            Effect::RequestModelPicker { .. } => self.notice(
                NoticeLevel::Info,
                format!("the model picker is {NOT_AVAILABLE}; use /model <provider>:<id> (anthropic, openai, gemini)"),
            ),
            Effect::Remember(_) => self.unavailable("/remember"),
            Effect::OpenMemory(_) | Effect::MemoryEdited { .. } => self.unavailable("/memory"),
            Effect::RespondEscalation(..)
            | Effect::AddRule { .. }
            | Effect::RemoveRule(_)
            | Effect::ListRules
            | Effect::TrustRealmRules
            | Effect::TrustProject
            | Effect::SetMode(_)
            | Effect::ResumeAutoMode => self.notice(
                NoticeLevel::Warn,
                format!("approvals are {NOT_AVAILABLE}: the daemon's orchestrator runs without the approval gate for now"),
            ),
            Effect::Quit
            | Effect::SaveUnsent(_)
            | Effect::CancelShell
            | Effect::SetChecklistView(_)
            | Effect::CostSnapshot(_)
            | Effect::Redraw
            | Effect::Suspend
            | Effect::ExternalEditor(_) => {}
        }
    }

    fn unavailable(&self, what: &str) {
        self.notice(NoticeLevel::Warn, format!("{what} is {NOT_AVAILABLE}"));
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
        self.catch_up_at = Some(Instant::now() + CATCH_UP_EVERY);
        let text = if text.contains('@') {
            crate::mentions::expand_mentions(text, &self.config.scope.project_root).text
        } else {
            text.to_string()
        };
        if let Some((model, provider)) = self.pending_model.take() {
            let reports = self.reports_tx.clone();
            tokio::spawn(async move {
                if let Err(err) =
                    remote::turn_with_model(&connection, &text, &model, provider.as_deref()).await
                {
                    let _ = reports.send(Report::TurnStart(err));
                }
            });
            return;
        }
        let sent =
            tokio::time::timeout(CALL_TIMEOUT, remote::send(&connection, &text, false)).await;
        let failure = match sent {
            Ok(Ok(())) => return,
            Ok(Err(err)) => err.to_string(),
            Err(_) => "the daemon did not take the prompt in time".to_string(),
        };
        self.turn.record(Err(TurnFailure::Failed(failure)));
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
        let sent = tokio::time::timeout(CALL_TIMEOUT, remote::send(&connection, &text, true)).await;
        let result = match sent {
            Ok(Ok(())) => {
                self.turn.steers.push_back(id);
                Ok(())
            }
            Ok(Err(err)) => Err(SteerFailure::Rejected(err.to_string())),
            Err(_) => Err(SteerFailure::Rejected(
                "the daemon did not answer in time".into(),
            )),
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
        let reports = self.reports_tx.clone();
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
                Some(run) => tokio::time::timeout(
                    CALL_TIMEOUT,
                    remote::stop_run(&connection, &run, INTERRUPT_REASON),
                )
                .await
                .map(|r| r.map(drop)),
                None => tokio::time::timeout(CALL_TIMEOUT, remote::force_cancel(&connection)).await,
            };
            let failure = match result {
                Ok(Ok(())) => return,
                Ok(Err(err)) => err.to_string(),
                Err(_) => "the daemon did not answer in time".to_string(),
            };
            let _ = reports.send(Report::Interrupt(failure));
        });
    }

    /// `/model <provider>:<id>` (or a catalogued bare id): the next prompt
    /// carries the switch (`mob/turn_start` with the identity overrides);
    /// the session keeps the model from then on.
    fn set_model(&mut self, reference: &str) {
        match parse_model(reference) {
            Ok((model, provider)) => {
                let shown = provider.clone().unwrap_or_default();
                self.notice(
                    NoticeLevel::Info,
                    format!("switching to {model} with your next message"),
                );
                self.set_model_status(&shown, &model);
                self.pending_model = Some((model, provider));
            }
            Err(why) => self.notice(NoticeLevel::Warn, why),
        }
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
        lines.push("approval gate: none on the orchestrator yet".to_string());
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

/// `/model` in attach mode: `<provider>:<id>` with a built-in provider, or a
/// bare id the catalog names a provider for.
///
/// # Errors
///
/// Why the reference cannot be switched to from attach mode.
pub fn parse_model(reference: &str) -> Result<(String, Option<String>), String> {
    let reference = reference.trim();
    if let Some((endpoint, id)) = reference.split_once(':') {
        return match WireDialect::parse(endpoint) {
            Some(dialect) if !id.trim().is_empty() => {
                Ok((id.trim().to_string(), Some(dialect.as_str().to_string())))
            }
            Some(_) => Err("usage: /model <provider>:<id>".to_string()),
            None => Err(format!(
                "attach mode switches only on the built-in endpoints (anthropic, openai, gemini) for now, not `{endpoint}`"
            )),
        };
    }
    match WireDialect::of_catalog_model(reference) {
        Some(dialect) => Ok((reference.to_string(), Some(dialect.as_str().to_string()))),
        None => Err(format!(
            "`{reference}` is not a catalogued model; name its provider: /model <provider>:{reference}"
        )),
    }
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
    match (launch::decide(launch::probe(file).await, mode), spawn) {
        (Decision::Attach(endpoint), _) => Ok(endpoint),
        (Decision::Start, Some(plan)) => {
            let _ = ui_tx.send(Msg::Status(StatusUpdate::Attach(Some(STARTING.into()))));
            let (pid, mut exited) = launch::spawn_daemon(&plan.program, &plan.argv, &plan.log)?;
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

/// Attach, find the orchestrator's session, open its live stream and read
/// what the saved cursor has not shown yet.
async fn establish(endpoint: Endpoint, saved: Option<&Cursor>) -> Result<Established, ClientError> {
    let (connection, notifications) = Connection::attach(&endpoint, ATTACH_TIMEOUT).await?;
    tokio::time::timeout(CALL_TIMEOUT, async {
        connection.call("initialize", serde_json::json!({})).await?;
        let session = remote::session_id(&connection).await?;
        let mark = Mark::resume(&session.to_string(), saved);
        // The live stream first, so nothing falls between it and the replay.
        let stream = remote::open_stream(&connection).await?;
        let mut replay = Vec::new();
        let mut after = mark.sequence();
        loop {
            let (page, more) = remote::replay(&connection, &session, after).await?;
            after = page.last().map_or(after, |envelope| envelope.seq);
            let empty = page.is_empty();
            replay.extend(page);
            if !more || empty {
                break;
            }
        }
        let identity = remote::identity(&connection, &session).await.ok();
        Ok(Established {
            link: Link {
                connection: Arc::new(connection),
                notifications,
                stream,
                session,
                endpoint,
            },
            mark,
            replay,
            identity,
        })
    })
    .await
    .map_err(|_| ClientError::Timeout("reading the orchestrator's state"))?
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn model_references_name_a_built_in_provider() {
        assert_eq!(
            parse_model("anthropic:claude-opus-4-8"),
            Ok(("claude-opus-4-8".into(), Some("anthropic".into())))
        );
        assert_eq!(
            parse_model(" claude-sonnet-4-6 "),
            Ok(("claude-sonnet-4-6".into(), Some("anthropic".into())))
        );
        assert!(
            parse_model("litellm:gpt-x")
                .unwrap_err()
                .contains("built-in")
        );
        assert!(parse_model("my-alias").unwrap_err().contains("provider"));
        assert!(parse_model("openai:").is_err());
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
