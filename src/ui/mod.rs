//! ratatui application: Elm-style state + pure `update`, a renderer and a terminal runtime.
//!
//! [`App::update`] is pure — it never touches the terminal or performs I/O — and
//! returns [`Effect`]s that the integrator (session/approval wiring) executes.
//! Everything the integrator wants to show is pushed back in as a [`Msg`].

pub mod checklist;
pub mod commands;
pub mod compaction;
pub mod complete;
pub mod connection;
pub mod cost;
pub mod delivery;
mod dispatch;
pub mod editor;
mod escalations;
mod helpers;
pub mod history;
pub mod input;
pub mod keymap;
mod keys;
pub mod markdown;
pub mod modal;
mod mouse;
pub mod peer;
pub mod render;
pub mod runtime;
pub mod search;
pub mod selection;
mod shell;
pub mod transcript;
mod turns;

use crate::approval::escalation::RecordedPrompt;
use crate::approval::mode::PermissionMode;
use crate::models::ModelGroup;
use crate::paths::ProjectRoot;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use meerkat_core::{AgentEvent, EventEnvelope, ScopedAgentEvent};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::layout::Rect;

pub use checklist::{Checklist, ChecklistView};
#[cfg(any(test, feature = "test-support"))]
pub use checklist::{ChecklistItem, ItemState};
pub use commands::{
    AutoCommand, Command, CompactCommand, HelperTarget, HelpersCommand, MemoryTarget, ModelCommand,
    RuleEffect, RulesCommand,
};
#[cfg(any(test, feature = "test-support"))]
pub use compaction::CompactMode;
pub use compaction::{CompactFailure, CompactionGauge, CompactionPolicy};
pub use connection::Connection;
#[cfg(any(test, feature = "test-support"))]
pub use cost::{CostConfidence, CostReadout, UsageTotals};
pub use cost::{SessionCost, SessionCostSnapshot};
pub use delivery::{Pending, PendingId, PendingQueue, PendingState, SteerFailure};
pub use history::History;
pub use input::{InputEditor, StashOutcome};
pub use keymap::{Action, EditAction};
pub use modal::{
    EscalationAnswer, EscalationDialog, EscalationId, Modal, ModalKey, ModelPicker, PlanChoice,
    PlanDialog, QuitChoice, QuitDialog, ResumePicker,
};
pub use runtime::{UiChannels, UiError, run_ui};
pub use search::{HistorySearch, SearchOutcome};
pub use selection::{CellPos, SelectMode, Selection};
#[cfg(any(test, feature = "test-support"))]
pub use transcript::QueuedLabel;
#[cfg(any(test, feature = "test-support"))]
pub use transcript::ToolState;
pub use transcript::{Block, BlockId, DiffView, HelperView, NoticeLevel, ShellState, Transcript};

/// Window in which a second Ctrl-C on an idle, empty prompt quits.
pub const DOUBLE_CTRL_C_WINDOW: Duration = Duration::from_millis(1500);

/// Window in which a second Ctrl-D confirms quitting (on an idle empty
/// input), or a second Ctrl-D / `/quit` while a turn is running or messages
/// are queued.
pub const QUIT_CONFIRM_WINDOW: Duration = Duration::from_secs(3);

/// Window in which a second Esc on an idle input with text clears it
/// (saving it to the prompt history).
pub const DOUBLE_ESC_WINDOW: Duration = Duration::from_millis(1500);

/// Window after Ctrl-X in which the second key of a chord is accepted.
pub const CHORD_WINDOW: Duration = Duration::from_millis(1500);

/// Window in which a second (third) left press on the same cell is a double
/// (triple) click.
pub const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// A left button press not yet released.
#[derive(Debug, Clone, Copy)]
struct Press {
    /// Cell pressed.
    pos: CellPos,
    /// Screen row pressed (for click-to-expand).
    row: u16,
    /// 1 = single, 2 = double, 3 = triple click.
    clicks: u8,
    /// Whether the pointer moved with the button held.
    dragged: bool,
}

/// The last left press, for multi-click detection.
#[derive(Debug, Clone, Copy)]
struct LastDown {
    at: Instant,
    row: u16,
    column: u16,
    clicks: u8,
}

/// How keys reach the input area.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum InputMode {
    /// Keys edit the prompt.
    #[default]
    Normal,
    /// Keys drive a reverse history search (Ctrl-R).
    Search(HistorySearch),
}

/// Result of a finished turn, reported by the runner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnSummary {
    /// Final assistant text.
    pub text: String,
    /// Input tokens consumed by the run.
    pub input_tokens: u64,
    /// Output tokens produced by the run.
    pub output_tokens: u64,
}

/// How a turn ended without a result. With `Ok(TurnSummary)` (completed)
/// these are the turn's typed ends.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TurnFailure {
    /// An input of the turn failed (the first failure wins).
    #[error("{0}")]
    Failed(String),
    /// The user interrupted the turn (the runtime reported the inputs
    /// cancelled).
    #[error("interrupted")]
    Interrupted,
}

impl TurnFailure {
    /// How much this end weighs against the turn's other runs' ends
    /// ([`merge_turn_end`]): a failure outranks an interruption, which
    /// outranks a completion (0).
    #[must_use]
    pub fn rank(end: &Result<TurnSummary, Self>) -> u8 {
        match end {
            Ok(_) => 0,
            Err(Self::Interrupted) => 1,
            Err(Self::Failed(_)) => 2,
        }
    }
}

/// Fold one run's end `next` into the turn's end so far, in both runners:
/// the higher [`TurnFailure::rank`] wins. On a tie a failure keeps the
/// earlier one (the first failure is the one to show) and a completion
/// takes the later one (the last run's text is the turn's answer).
pub fn merge_turn_end(
    current: Option<Result<TurnSummary, TurnFailure>>,
    next: Result<TurnSummary, TurnFailure>,
) -> Result<TurnSummary, TurnFailure> {
    match current {
        Some(current) if TurnFailure::rank(&current) > TurnFailure::rank(&next) => current,
        Some(current)
            if TurnFailure::rank(&current) == TurnFailure::rank(&next) && current.is_err() =>
        {
            current
        }
        _ => next,
    }
}

/// Result of a turn (the started prompt and every follow-up run the runtime
/// added for steered messages) plus the steered messages it never delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// The turn's result: completed, or how it ended without one.
    pub result: Result<TurnSummary, TurnFailure>,
    /// Steered messages the runtime ended without writing them into the
    /// conversation (the turn was interrupted or failed), oldest first. A
    /// message already shown as delivered can be listed here when its run
    /// was discarded.
    pub undelivered: Vec<PendingId>,
}

impl TurnOutcome {
    /// An outcome with nothing undelivered.
    #[must_use]
    pub fn plain(result: Result<TurnSummary, TurnFailure>) -> Self {
        Self {
            result,
            undelivered: Vec::new(),
        }
    }
}

/// An approval decision to show in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalNotice {
    /// Tool name.
    pub tool: String,
    /// Tier / mechanism that decided (e.g. `rule`, `classifier`, `user`).
    pub tier: String,
    /// Whether the call was allowed.
    pub allowed: bool,
    /// Optional explanation.
    pub reason: Option<String>,
}

/// Status-bar update pushed by the integrator.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusUpdate {
    /// Provider and model changed.
    Model {
        /// Provider name.
        provider: String,
        /// Model id.
        model: String,
    },
    /// Active session id (shortened for display).
    Session(String),
    /// Absolute token counters (e.g. after resuming).
    Tokens {
        /// Input tokens.
        input: u64,
        /// Output tokens.
        output: u64,
    },
    /// The permission mode, auto mode's pause and the denial counters.
    Mode {
        /// The permission mode.
        mode: PermissionMode,
        /// Whether auto mode is paused after too many classifier blocks.
        paused: bool,
        /// `(consecutive, consecutive_limit, total, total_limit)`.
        denials: (u32, u32, u32, u32),
    },
    /// Whether the classifier is currently running.
    Classifying(bool),
    /// Current context usage (tokens in the last model call); `approx` after
    /// compaction or resume when the number is an estimate.
    Context {
        /// Tokens used, `None` when unknown.
        used: Option<u64>,
        /// Whether `used` is an estimate.
        approx: bool,
    },
    /// The model's context window in tokens, `None` when unknown.
    ContextWindow(Option<u64>),
    /// The helpers (mob members) running now.
    Helpers(Vec<crate::mobs::HelperStatus>),
    /// Replace the session's cost totals (new or resumed session).
    CostTotals(SessionCostSnapshot),
    /// The daemon connection's state, shown first in the status bar.
    Connection(Connection),
    /// The agent this client works with (`main-<id>`), shown
    /// after the connection's state.
    Agent(Option<String>),
    /// The price table changed (start-up, fetched lists); the cost readout
    /// is recomputed.
    Prices(Arc<crate::pricing::PriceTable>),
}

/// Text queued for the system clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedCopy {
    /// The text, capped to the clipboard limit.
    pub text: String,
    /// Whether the cap cut it.
    pub truncated: bool,
}

/// What the user's external editor opens ([`LocalEffect::ExternalEditor`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorTarget {
    /// An instruction file (`/memory`), edited in place.
    Memory {
        /// Which one.
        target: MemoryTarget,
        /// Its path.
        path: std::path::PathBuf,
    },
    /// The input draft (Ctrl-G, Ctrl-X Ctrl-E), through a temporary file.
    Draft(String),
}

/// How an external editor run ended ([`Msg::EditorClosed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorClosed {
    /// An instruction file; `Err` says why the edit may not have been saved.
    Memory {
        /// Which one.
        target: MemoryTarget,
        /// Its path.
        path: std::path::PathBuf,
        /// The run's outcome.
        result: Result<(), String>,
    },
    /// The draft: its new text, or why the input is kept unchanged.
    Draft(Result<String, String>),
}

/// Input to [`App::update`].
#[derive(Debug, Clone)]
pub enum Msg {
    /// A key press.
    Key(KeyEvent),
    /// Bracketed paste.
    Paste(String),
    /// Terminal resized (the next draw lays out for the new size).
    Resize,
    /// Mouse event (only when mouse capture is enabled).
    Mouse(MouseEvent),
    /// Primary agent stream event.
    Agent(EventEnvelope<AgentEvent>),
    /// Scoped (sub-agent) stream event.
    Scoped(ScopedAgentEvent),
    /// The turn started by [`Effect::StartTurn`] finished.
    TurnFinished(TurnOutcome),
    /// The runner's answer to [`Effect::Steer`].
    SteerAck {
        /// The queued message.
        id: PendingId,
        /// `Ok` when the runtime admitted the message.
        result: Result<(), SteerFailure>,
    },
    /// The runtime wrote a steered message into the conversation (at a model
    /// boundary of the running turn, or as a follow-up turn).
    SteerDelivered(PendingId),
    /// The runner's answer to [`Effect::RecallQueued`]: the steered
    /// messages the runtime abandoned before they joined the run (oldest
    /// first; back to the editor), and the cancels that failed. Admitted
    /// messages not listed already reached the agent.
    Recalled {
        /// Abandoned messages.
        recalled: Vec<PendingId>,
        /// Failed cancels.
        errors: Vec<String>,
    },
    /// A file tool produced a diff.
    Diff {
        /// Tool-use id of the producing call.
        tool_use_id: String,
        /// File path.
        path: String,
        /// Unified diff.
        unified: String,
        /// Added lines.
        added: usize,
        /// Removed lines.
        removed: usize,
    },
    /// The approval engine needs a human decision.
    Escalation(RecordedPrompt, EscalationId),
    /// An escalation was resolved elsewhere (timeout, dropped); close it.
    EscalationResolved(EscalationId),
    /// An approval decision to log in the transcript.
    Approval(ApprovalNotice),
    /// A dim one-line note under a tool call's block (a session call's or a
    /// helper's), e.g. `allowed by classifier: <reason>`.
    ToolNote {
        /// Tool-use id of the call, as the model issued it.
        tool_use_id: String,
        /// The note.
        text: String,
    },
    /// One safety-classifier model call's usage: counted in the session's
    /// cost under the classifier model.
    ClassifierUsage {
        /// Classifier model id.
        model: String,
        /// Its usage.
        usage: meerkat_core::Usage,
    },
    /// A notice to show in the transcript.
    Notice {
        /// Severity.
        level: NoticeLevel,
        /// Text.
        text: String,
    },
    /// Status-bar update.
    Status(StatusUpdate),
    /// Open the resume picker with `(ref, label)` items.
    ShowResumePicker(Vec<(String, String)>),
    /// Open the model picker (answer to [`Effect::RequestModelPicker`]): one
    /// group per endpoint, the current endpoint and model first; a group's
    /// warning says why its list is missing.
    ShowModelPicker(Vec<ModelGroup>),
    /// Replace the transcript (e.g. after resuming a session).
    ResetTranscript(Vec<Block>),
    /// A turn this UI did not start runs on the daemon (another
    /// client's, or one the catch-up found in progress): show `prompt` as
    /// the user's message when there is one and count the UI busy until
    /// [`Msg::TurnFinished`], as if it had started it.
    RemoteTurn {
        /// The run's prompt (`None`: a continuation without one).
        prompt: Option<String>,
    },
    /// Another agent of the workspace messaged this one:
    /// shown as a block labelled with the sender, so the user sees why the
    /// agent woke.
    Peer {
        /// The sending agent's name.
        from: String,
        /// The message (empty: delivered into a running turn).
        text: String,
    },
    /// The daemon says the current run was fired by a schedule (or a file
    /// watch), not typed by a user: its prompt is shown labelled
    /// (`scheduled: <name>`).
    Triggered {
        /// What fired it.
        label: String,
        /// The run's input.
        prompt: String,
    },
    /// `/compact now`: the engine took the request, so the session's next
    /// compaction check compacts (or why it did not). The
    /// `CompactionStarted`/`Completed` events confirm the compaction itself;
    /// `RequestedCompactionSkipped` says why nothing was compacted.
    CompactRequested(Result<(), CompactFailure>),
    /// The terminal gained (`true`) or lost (`false`) the focus.
    Focus(bool),
    /// The integrator resolved `/memory`'s file (and created a missing
    /// `SYSTEM.md`): open it in the external editor.
    EditFile {
        /// Which instruction file.
        target: MemoryTarget,
        /// Its path.
        path: std::path::PathBuf,
    },
    /// The UI loop's answer to [`LocalEffect::ExternalEditor`].
    EditorClosed(EditorClosed),
    /// The runner's answer to [`Effect::RunShell`] (or to
    /// [`Effect::CancelShell`]).
    ShellFinished {
        /// The run's id.
        id: ShellRunId,
        /// How it went.
        outcome: ShellOutcome,
    },
    /// The current session's task checklist, read from the engine's task
    /// store (when a session opens and after each of its task tool calls).
    Checklist(Checklist),
    /// The task store could not be read: the checklist stays as it is and
    /// the next Ctrl+T says why.
    ChecklistUnavailable(String),
    /// What a prompt's `@` mentions attached as the runner sent it, and
    /// the sealed paths it named but never read (reported by the
    /// workbench, so the notice says what the agent actually got).
    Mentions {
        /// The workspace-relative paths attached, a block each.
        attached: Vec<String>,
        /// The sealed paths mentioned, not read.
        sealed: Vec<String>,
    },
    /// The UI loop's answer to [`LocalEffect::IndexPaths`].
    PathIndex {
        /// The request it answers (an older one's answer is dropped).
        generation: u64,
        /// The workspace's paths.
        paths: Vec<crate::mentions::PathEntry>,
    },
    /// The runner's answer to [`Effect::DecideQuit`].
    QuitDecided(QuitDecision),
    /// A re-home moved this client to another workspace (`/rehome`): `@`
    /// mentions complete there, the `!` shell's confinement may differ,
    /// and the input history and drafts are the realm's at `realm`.
    Rerooted {
        /// The workspace.
        workspace: ProjectRoot,
        /// Its realm directory.
        realm: std::path::PathBuf,
        /// Whether the `!` shell runs confined there.
        shell_confined: bool,
    },
    /// The terminal hung up (SIGHUP): leave at once, as a detach (the
    /// daemon keeps running; unsent messages are saved).
    Hangup,
    /// Animation tick.
    Tick,
}

/// How an attached `/quit` left ([`Msg::QuitDecided`]): what the daemon
/// did (`rkat/quit` decides and does it), or that the user is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuitDecision {
    /// This client started the daemon, was the last one and no agent was
    /// left running without a client: the daemon stops (every agent
    /// retires).
    Terminate,
    /// Other clients work with this client's agent: it left, the agent
    /// stays for them (or the daemon was stopped already).
    Leave,
    /// Other clients are attached to other agents: this client's agent
    /// retired, the daemon stays for them.
    Retire,
    /// Ask: detach or terminate.
    Ask {
        /// Why the daemon is not just stopped.
        reason: String,
        /// A turn is running (terminating interrupts it).
        running: bool,
    },
}

/// What leaving does to the attached daemon ([`Effect::Leave`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Departure {
    /// `/detach`: keep the agent running and record the detach.
    Detach,
    /// Stop the daemon and retire every agent (`rkat/shutdown`).
    Terminate,
}

/// Identifies one `!` command run ([`Effect::RunShell`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShellRunId(pub u64);

/// How a `!` command run ended ([`Msg::ShellFinished`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellOutcome {
    /// It ran; `context` says whether the session took its output.
    Ran {
        /// The run.
        run: crate::user_shell::ShellRun,
        /// `Err`: why its output could not join the context.
        context: Result<(), String>,
    },
    /// It could not start.
    Failed(String),
    /// The user cancelled it.
    Cancelled,
}

/// Notice for `/compact <instructions>`: the engine cannot use them.
pub const COMPACT_FOCUS_UNSUPPORTED: &str =
    "focus instructions are not supported by the engine yet; compacting without them";

/// Notice shown once the engine took a `/compact` request.
pub const COMPACT_REQUESTED: &str = "the context will be compacted at the start of your next message (the ✓ compacted notice confirms it)";

/// Side effect requested by [`App::update`], executed by the integrator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Run a turn with this prompt; answer with [`Msg::TurnFinished`].
    StartTurn(String),
    /// Run a `!` command under the agent shell's confinement and add its
    /// output to the session's context; answer with [`Msg::ShellFinished`].
    RunShell {
        /// The run's id.
        id: ShellRunId,
        /// The command line.
        command: String,
    },
    /// Cancel the running `!` command (answered with
    /// [`ShellOutcome::Cancelled`] unless it already finished).
    CancelShell,
    /// Interrupt the running turn. Ctrl-X Ctrl-S and Ctrl+Enter with a
    /// draft send it too: the UI keeps the draft and starts it as the next
    /// turn itself once [`Msg::TurnFinished`] arrives.
    Interrupt,
    /// Pull back every steered message the runtime still holds queued (not
    /// yet joined to the run) without touching the run; answered with
    /// [`Msg::Recalled`].
    RecallQueued,
    /// Steer a message into the running turn (a durable `Steer` input);
    /// answered with [`Msg::SteerAck`], then [`Msg::SteerDelivered`] or the
    /// turn's [`TurnOutcome::undelivered`].
    Steer {
        /// Queue id allocated by the UI.
        id: PendingId,
        /// Raw user text, sent as an ordinary user message.
        text: String,
    },
    /// Leave the application. The integrator interrupts a running turn,
    /// persists the session and saves undelivered messages before exiting.
    /// Leaves the daemon running (a hangup is a detach).
    Quit,
    /// Sent right before [`Effect::Quit`]: what leaving does
    /// to the daemon (`rkat/detach` or `rkat/shutdown`).
    Leave(Departure),
    /// `/quit` asks the runner how to leave; answered with
    /// [`Msg::QuitDecided`].
    DecideQuit,
    /// Sent right before [`Effect::Quit`]: every message the UI still holds
    /// (queued, deferred, or the Ctrl-X Ctrl-S prompt as `None`). The
    /// integrator saves those the model never received.
    SaveUnsent(Vec<(Option<PendingId>, String)>),
    /// Start a fresh session.
    NewSession,
    /// Resume a session by reference (`last`, `~N`, id prefix).
    ResumeSession(String),
    /// Load session list; answer with [`Msg::ShowResumePicker`].
    RequestResumePicker,
    /// `/rewind` (Esc Esc on an empty idle input): the client says why the
    /// daemon cannot rewind a conversation (an engine ask).
    RequestRewindMenu,
    /// Answer an escalation.
    RespondEscalation(EscalationId, EscalationAnswer),
    /// Persist an approval rule.
    AddRule {
        /// Allow or deny.
        effect: RuleEffect,
        /// Tool name.
        tool: String,
        /// Optional argument pattern.
        pattern: Option<String>,
    },
    /// Remove rule number `n`.
    RemoveRule(usize),
    /// List rules; answer with [`Msg::Notice`].
    ListRules,
    /// `/permissions trust`: show the realm `tui.toml` and record its digest in
    /// the host trust record; answered with [`Msg::Notice`].
    TrustRealmRules,
    /// `/trust`: record every project input's digest
    /// in the host trust record; their servers and hooks run from the next
    /// session.
    TrustProject,
    /// Switch the permission mode (`Shift+Tab`, `/mode`, `/auto on|off`).
    SetMode(PermissionMode),
    /// Resume auto mode after a denial-limit pause.
    ResumeAutoMode,
    /// Record the checklist panel's view (Ctrl+T) as `[ui] checklist` in
    /// the realm `tui.toml`.
    SetChecklistView(ChecklistView),
    /// Switch model.
    SetModel(String),
    /// Load the provider's model list (cached for the session unless
    /// `refresh`); answer with [`Msg::ShowModelPicker`].
    RequestModelPicker {
        /// Refetch instead of using the cached list.
        refresh: bool,
    },
    /// Append a line to the workspace AGENTS.md and push it live.
    Remember(String),
    /// `/compact now` (idle only): ask the engine to compact at the
    /// session's next model call; answered with [`Msg::CompactRequested`].
    CompactNow,
    /// The session's cost totals after a model call; the integrator
    /// persists them next to the session.
    CostSnapshot(SessionCostSnapshot),
    /// `/context`: the UI's part of the report (the gauge and the
    /// compaction policy); the integrator adds what the context holds and
    /// answers with one [`Msg::Notice`].
    ContextReport(String),
    /// `/export [file]`: write `text` (the transcript as plain text) to
    /// `path` (relative to the workspace), or to a fresh file in the
    /// workspace's realm when `None`; answered with a notice.
    Export {
        /// Where to write, as typed.
        path: Option<String>,
        /// The transcript text.
        text: String,
    },
    /// `/status`: the integrator answers with one [`Msg::Notice`] naming
    /// the version, model, session, workspace, realm, confinement, auto
    /// mode, trust state and helper depth limit.
    StatusReport,
    /// `/daemon stop`: ask the attached daemon to shut down (
    /// answered with a notice).
    DaemonStop,
    /// `/rehome`: list the state this workspace could adopt (`None`), or
    /// re-home: the runner stops the daemon, moves the realm, re-roots
    /// itself and reattaches, telling the UI with [`Msg::Rerooted`].
    Rehome(Option<String>),
    /// `/daemon start`: connect to the workspace's daemon again (starting
    /// it when this client may) after the connect failed or the daemon
    /// stopped (answered with notices and the connection's state).
    DaemonStart,
    /// `/agents`: list the workspace's agents (`None`), or switch this
    /// client to the agent named (answered with notices).
    Agents(Option<String>),
    /// `/audit [n]`: show the daemon's audit log's last lines for this
    /// client's agent (answered with a notice).
    Audit(Option<usize>),
    /// `/schedules`: list this agent's schedules, or act on one (answered
    /// with a notice).
    Schedules(Option<(crate::daemon::protocol::ScheduleOp, String)>),
    /// `/memory`: resolve the instruction file (creating `SYSTEM.md` with
    /// a comment header) and answer with [`Msg::EditFile`].
    OpenMemory(MemoryTarget),
    /// An instruction file was edited (`/memory`): push its new content to
    /// the live session where it can go, or say it applies from the next.
    MemoryEdited {
        /// Which instruction file.
        target: MemoryTarget,
        /// Its path.
        path: std::path::PathBuf,
    },
}

/// What the UI loop ([`run_ui`]) does itself after an update; none of it
/// reaches the integrator. Drained with [`App::take_local`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalEffect {
    /// A line was submitted with Enter (a prompt or a slash command, as
    /// typed; never blank): append it to the prompt histories.
    Submitted(String),
    /// Put this text on the system clipboard (OSC 52, after the next
    /// frame); the loop reports how it went as a notice
    /// ([`selection::copy_notice`]).
    Copy(QueuedCopy),
    /// Run the user's editor on `target` (leaving TUI mode around it); the
    /// loop answers with [`Msg::EditorClosed`].
    ExternalEditor(EditorTarget),
    /// Ctrl-Z: give the terminal back and stop the process (SIGTSTP); on
    /// `fg`, take the terminal again and repaint. A no-op off Unix.
    Suspend,
    /// Ctrl-L: clear the terminal and repaint everything.
    Redraw,
    /// List the workspace's paths for the `@` completion menu off the UI
    /// thread ([`crate::mentions::workspace_paths`] walks up to
    /// [`crate::mentions::MAX_INDEX_PATHS`]); the loop answers with
    /// [`Msg::PathIndex`].
    IndexPaths {
        /// Identifies this request.
        generation: u64,
        /// The workspace to walk.
        root: ProjectRoot,
    },
    /// The realm changed (a re-home): the input history and the drafts
    /// are this realm directory's from now on.
    Reroot(std::path::PathBuf),
    /// Ring the terminal bell (after the next frame).
    Bell,
    /// The state changed: draw a frame.
    Draw,
}

/// Everything the status bar shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusInfo {
    /// Provider name.
    provider: String,
    /// Model id.
    model: String,
    /// Short session id.
    session_short: String,
    /// Cumulative input tokens.
    input_tokens: u64,
    /// Cumulative output tokens.
    output_tokens: u64,
    /// The permission mode (`None`, before the runner reports it, hides
    /// the segment).
    mode: Option<PermissionMode>,
    /// Whether auto mode is paused after too many classifier blocks.
    auto_paused: bool,
    /// `(consecutive, consecutive_limit, total, total_limit)` denials.
    denials: (u32, u32, u32, u32),
    /// When the running turn started.
    running_since: Option<Instant>,
    /// Number of messages typed mid-turn that have not been delivered yet.
    queued: usize,
    /// Number of helpers running now.
    helpers: usize,
    /// Whether the helper panel lists any helper (the status bar names the
    /// key that selects them).
    helper_panel: bool,
    /// Whether the classifier is running.
    classifying: bool,
    /// Tokens in the last model call, `None` when unknown.
    context_used: Option<u64>,
    /// Model context window in tokens, `None` when unknown.
    context_window: Option<u64>,
    /// Whether `context_used` is an estimate.
    context_approx: bool,
    /// Whether `context_window` is an estimate (model not in the catalog).
    window_approx: bool,
    /// Token totals per model, for the cost readout.
    cost: SessionCost,
    /// Prices for the cost readout.
    prices: Arc<crate::pricing::PriceTable>,
    /// The daemon connection's state; `None` until the runner reports
    /// one.
    connection: Option<Connection>,
    /// The agent this client works with.
    agent: Option<String>,
}

/// The prompt the "Plan ready" dialog's proceed choices send.
pub const PROCEED_PROMPT: &str = "Proceed with the plan";

/// The model a helper's usage is counted under when neither the provider
/// nor the helper host named one.
const UNKNOWN_HELPER_MODEL: &str = "unknown";

/// Number of session-id characters shown in the status bar.
const SESSION_SHORT_LEN: usize = 8;

impl StatusInfo {
    /// Whether this client reached the daemon ([`Connection::reached`]).
    #[must_use]
    pub fn reached_daemon(&self) -> bool {
        self.connection.as_ref().is_some_and(Connection::reached)
    }

    /// Add to the token counters (a model call's usage).
    fn add_tokens(&mut self, input: u64, output: u64) {
        self.input_tokens = self.input_tokens.saturating_add(input);
        self.output_tokens = self.output_tokens.saturating_add(output);
    }

    /// Token totals per model, for the cost readout.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn cost(&self) -> &SessionCost {
        &self.cost
    }

    /// Prices for the cost readout.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn prices(&self) -> &crate::pricing::PriceTable {
        &self.prices
    }

    /// Number of messages typed mid-turn that have not been delivered yet.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queued
    }

    /// The mode as `/auto` and `/mode` describe it.
    #[must_use]
    pub fn mode_text(&self) -> String {
        let (c, cm, t, tm) = self.denials;
        match self.mode {
            None => "unknown".to_string(),
            Some(PermissionMode::Auto) => format!(
                "auto{} (consecutive denials {c}/{cm}, total {t}/{tm})",
                if self.auto_paused { ", PAUSED" } else { "" }
            ),
            Some(mode) => mode.label().to_string(),
        }
    }

    /// Apply an integrator update: the only way in from outside the UI.
    pub fn apply(&mut self, update: StatusUpdate) {
        match update {
            StatusUpdate::Model { provider, model } => {
                self.provider = provider;
                self.model = model;
            }
            StatusUpdate::Session(id) => {
                self.session_short = id.chars().take(SESSION_SHORT_LEN).collect();
            }
            StatusUpdate::Tokens { input, output } => {
                self.input_tokens = input;
                self.output_tokens = output;
            }
            StatusUpdate::Mode {
                mode,
                paused,
                denials,
            } => {
                self.mode = Some(mode);
                self.auto_paused = paused;
                self.denials = denials;
            }
            StatusUpdate::Classifying(c) => self.classifying = c,
            StatusUpdate::Context { used, approx } => {
                self.context_used = used;
                self.context_approx = approx;
            }
            StatusUpdate::ContextWindow(window) => {
                self.context_window = window;
                self.window_approx = false;
            }
            StatusUpdate::Helpers(helpers) => self.helpers = helpers.len(),
            StatusUpdate::CostTotals(cost) => self.cost = cost,
            StatusUpdate::Prices(prices) => self.prices = prices,
            StatusUpdate::Connection(state) => self.connection = Some(state),
            StatusUpdate::Agent(agent) => self.agent = agent,
        }
    }
}

/// Longest quoted excerpt of a message in a notice, in characters.
const QUOTE_MAX_CHARS: usize = 40;

/// `“text”`, shortened to its first line and [`QUOTE_MAX_CHARS`] characters.
fn quote_short(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(QUOTE_MAX_CHARS).collect();
    if out.len() < line.len() || text.trim().lines().count() > 1 {
        out.push('…');
    }
    format!("“{out}”")
}

/// Tokens in the context after a model call: everything sent plus the reply.
pub(crate) fn context_tokens(usage: &meerkat_core::Usage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.output_tokens)
        .saturating_add(usage.cache_creation_tokens.unwrap_or(0))
        .saturating_add(usage.cache_read_tokens.unwrap_or(0))
}

/// An open helper full view: the helper's header and the view's own
/// scroll position (it follows new output while at the bottom).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpenHelperView {
    header: BlockId,
    scroll: ScrollState,
}

/// Transcript scroll position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollState {
    /// Lines between the bottom of the viewport and the end of the transcript.
    pub offset_from_bottom: usize,
    /// Whether the view sticks to the bottom as output arrives.
    pub follow: bool,
    last_total: usize,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            offset_from_bottom: 0,
            follow: true,
            last_total: 0,
        }
    }
}

impl ScrollState {
    /// Scroll towards older output.
    pub fn up(&mut self, lines: usize) {
        self.offset_from_bottom = self.offset_from_bottom.saturating_add(lines);
        self.follow = false;
    }

    /// Scroll towards newer output; reaching the bottom re-enables follow.
    pub fn down(&mut self, lines: usize) {
        self.offset_from_bottom = self.offset_from_bottom.saturating_sub(lines);
        if self.offset_from_bottom == 0 {
            self.follow = true;
        }
    }

    /// Jump to the oldest output.
    pub fn top(&mut self) {
        self.offset_from_bottom = usize::MAX;
        self.follow = false;
    }

    /// Jump to the newest output and follow.
    pub fn bottom(&mut self) {
        self.offset_from_bottom = 0;
        self.follow = true;
    }

    /// Reconcile with the laid-out transcript and return the first visible line.
    ///
    /// When not following, growth at the bottom shifts the offset so the visible
    /// window stays put.
    pub fn window(&mut self, total: usize, height: usize) -> usize {
        if self.follow {
            self.offset_from_bottom = 0;
        } else if total > self.last_total && self.last_total > 0 {
            self.offset_from_bottom = self
                .offset_from_bottom
                .saturating_add(total - self.last_total);
        }
        self.last_total = total;
        let max_offset = total.saturating_sub(height);
        self.offset_from_bottom = self.offset_from_bottom.min(max_offset);
        if self.offset_from_bottom == 0 {
            self.follow = true;
        }
        total.saturating_sub(height) - self.offset_from_bottom
    }
}

/// The `@` menu's workspace paths.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PathIndex {
    /// None held (no path menu open).
    Dropped,
    /// Asked for with this generation ([`LocalEffect::IndexPaths`]).
    Indexing(u64),
    /// The paths.
    Ready(Vec<crate::mentions::PathEntry>),
}

/// What keeps a session-level command from running now
/// ([`App::blocker`]). Its `Display` is the notice that says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Busy {
    /// A turn is running.
    Turn,
    /// A `/compact` request is on its way to the engine.
    Compacting,
    /// A `!` command is running.
    Shell,
    /// Messages wait to be delivered (queued, sent next, or being recalled).
    Queued,
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Turn => "a turn is running; interrupt it first (Ctrl-C)",
            Self::Compacting => "/compact is still waiting for the engine; wait for its answer",
            Self::Shell => "a ! command is running; wait for it or cancel it (Esc)",
            Self::Queued => {
                "messages are still queued; wait for them or take them back (↑ on an empty input)"
            }
        })
    }
}

/// Application state.
///
/// `App` is pure: it performs no I/O. Prompt-history persistence is the
/// integrator's job — seed recall with [`App::with_history`] (e.g. from
/// [`History::load`]) and, after each [`App::update`], drain newly submitted
/// lines with [`App::take_submitted`] and append them to the [`History`] file.
#[derive(Debug)]
pub struct App {
    /// Rendered conversation.
    pub transcript: Transcript,
    /// Prompt editor.
    pub input: InputEditor,
    /// Transcript scroll position.
    pub scroll: ScrollState,
    /// Status-bar data.
    pub status: StatusInfo,
    /// Open modal, if any.
    pub modal: Option<Modal>,
    /// Messages typed while a turn was running, not yet delivered.
    pending: PendingQueue,
    /// Prompt to send once the interrupted turn ends (Ctrl-X Ctrl-S).
    send_next: Option<String>,
    /// Sequence number of the last primary-stream event processed.
    last_seq: u64,
    /// Normal editing or history search.
    mode: InputMode,
    /// When Ctrl-X was pressed, while a chord is pending.
    chord: Option<Instant>,
    /// Whether `/compact now` is asking for a compaction ([`Effect::CompactNow`]
    /// sent, no [`Msg::CompactRequested`] yet). A prompt sent meanwhile is
    /// admitted as an ordinary `Queue` input once the request is answered
    /// (the runner serves effects in order); session switches wait.
    compacting: bool,
    /// When a Ctrl-D / `/quit` was refused because work is in flight; a
    /// second one within [`QUIT_CONFIRM_WINDOW`] quits anyway.
    quit_confirm: Option<Instant>,
    /// An [`Effect::DecideQuit`] waits for its
    /// [`Msg::QuitDecided`] (another `/quit` meanwhile leaves at once).
    deciding_quit: bool,
    /// Context-gauge policy: warn about a filling context (warn mode).
    gauge: CompactionGauge,
    /// The history estimate of a compaction that started and has not
    /// completed: the input of its summary call, counted on completion.
    compaction_input: Option<u64>,
    /// Whether assistant text streamed during the current turn (the turn
    /// summary is shown only when nothing did).
    streamed_text: bool,
    /// Global (cross-workspace) prompt history, oldest first; searched after the
    /// workspace history.
    global_history: Vec<String>,
    /// Search corpus (newest first, deduplicated), built when a search starts.
    search_corpus: Vec<String>,
    pending_escalations: VecDeque<(EscalationId, RecordedPrompt)>,
    /// A picker ([`Modal::Model`] or [`Modal::Resume`]) an escalation
    /// displaced; restored once no escalation remains.
    parked: Option<Modal>,
    submitted: Vec<String>,
    running: bool,
    run_first_block: usize,
    last_ctrl_c: Option<Instant>,
    /// When an Esc on an idle input with text armed the draft clear.
    last_esc: Option<Instant>,
    viewport_height: usize,
    /// Screen area of the transcript and, per drawn row, the block and the
    /// transcript logical line shown there, as of the last draw (for mouse
    /// hit-testing and selection).
    transcript_area: Rect,
    transcript_rows: Vec<(BlockId, usize)>,
    /// Mouse text selection (logical line coordinates).
    selection: Option<Selection>,
    press: Option<Press>,
    last_down: Option<LastDown>,
    /// Text to put on the system clipboard (drained by the runtime).
    clipboard: Option<QueuedCopy>,
    /// The loop's own effects queued in order ([`LocalEffect::ExternalEditor`],
    /// [`LocalEffect::Suspend`], [`LocalEffect::Redraw`]); the others are
    /// kept in their own fields and coalesced.
    local: Vec<LocalEffect>,
    /// The helper header selected in the helper panel (Ctrl+Up/Down; Enter
    /// on an empty input opens its full view).
    panel_selected: Option<BlockId>,
    /// The helper whose full view replaces the transcript, if one is open.
    helper_view: Option<OpenHelperView>,
    /// An [`Effect::RecallQueued`] is waiting for its [`Msg::Recalled`].
    recalling: bool,
    /// Admitted messages a recall found already with the agent: ↑ no
    /// longer tries to recall them (it recalls history).
    reached: Vec<PendingId>,
    /// Ring the bell when a turn ends or an approval prompt opens while the
    /// terminal is unfocused (`[ui] bell`).
    bell: bool,
    /// Whether the terminal has the focus (assumed at start-up).
    focused: bool,
    /// A bell is due ([`Self::take_bell`]).
    ring: bool,
    /// The completion menu above the input, while open.
    menu: Option<complete::Menu>,
    /// The word Esc closed the menu for: it stays closed until the word
    /// changes.
    menu_dismissed: Option<complete::Word>,
    /// The workspace `@` mentions complete and resolve against (`None`:
    /// no path menu, no mention notices).
    workspace: Option<ProjectRoot>,
    /// The workspace paths, asked for when a path menu opens and dropped
    /// when it closes.
    path_index: PathIndex,
    /// The last [`LocalEffect::IndexPaths`] generation.
    path_index_generation: u64,
    /// The `!` command running now, if any.
    shell_run: Option<RunningShell>,
    /// The last allocated [`ShellRunId`].
    last_shell_id: u64,
    /// Whether `!` commands run confined (the editor title says so).
    shell_confined: bool,
    /// The current session's tasks (the panel under the input, Ctrl+T).
    checklist: Checklist,
    /// Whether the checklist panel shows (`[ui] checklist`).
    checklist_view: ChecklistView,
    /// Why the task store could not be read, said once at the next Ctrl+T.
    checklist_note: Option<String>,
    dirty: bool,
}

/// A `!` command on its way ([`Effect::RunShell`] sent, no
/// [`Msg::ShellFinished`] yet).
#[derive(Debug, Clone)]
struct RunningShell {
    id: ShellRunId,
    block: BlockId,
    command: String,
    since: Instant,
}

impl App {
    /// Create an app with the given status-bar data.
    pub fn new(status: StatusInfo) -> Self {
        Self {
            transcript: Transcript::new(),
            input: InputEditor::new(),
            scroll: ScrollState::default(),
            status,
            modal: None,
            pending: PendingQueue::new(),
            send_next: None,
            last_seq: 0,
            mode: InputMode::Normal,
            chord: None,
            compacting: false,
            quit_confirm: None,
            deciding_quit: false,
            gauge: CompactionGauge::default(),
            compaction_input: None,
            streamed_text: false,
            global_history: Vec::new(),
            search_corpus: Vec::new(),
            pending_escalations: VecDeque::new(),
            parked: None,
            submitted: Vec::new(),
            running: false,
            run_first_block: 0,
            last_ctrl_c: None,
            last_esc: None,
            viewport_height: 20,
            transcript_area: Rect::default(),
            transcript_rows: Vec::new(),
            selection: None,
            press: None,
            last_down: None,
            clipboard: None,
            local: Vec::new(),
            panel_selected: None,
            helper_view: None,
            recalling: false,
            reached: Vec::new(),
            bell: true,
            focused: true,
            ring: false,
            menu: None,
            menu_dismissed: None,
            workspace: None,
            path_index: PathIndex::Dropped,
            path_index_generation: 0,
            shell_run: None,
            last_shell_id: 0,
            shell_confined: true,
            checklist: Checklist::default(),
            checklist_view: ChecklistView::default(),
            checklist_note: None,
            dirty: true,
        }
    }

    /// The workspace `@` mentions complete against (the path menu).
    #[must_use]
    pub fn with_workspace(mut self, root: ProjectRoot) -> Self {
        self.workspace = Some(root);
        self
    }

    /// Whether to ring the bell when a turn ends or an approval prompt opens
    /// while the terminal is unfocused (default on).
    #[must_use]
    pub fn with_bell(mut self, bell: bool) -> Self {
        self.bell = bell;
        self
    }

    /// Whether a bell is due; clears it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.ring)
    }

    /// Ask for a bell when enabled and the terminal is unfocused.
    fn notify(&mut self) {
        if self.bell && !self.focused {
            self.ring = true;
        }
    }

    /// Whether the task checklist starts shown (`[ui] checklist`).
    #[must_use]
    pub fn with_checklist_view(mut self, view: ChecklistView) -> Self {
        self.checklist_view = view;
        self
    }

    /// The current session's task checklist.
    pub fn checklist(&self) -> &Checklist {
        &self.checklist
    }

    /// The checklist the panel under the input shows: `Some` while the
    /// panel is on and the session has tasks.
    pub fn checklist_panel(&self) -> Option<&Checklist> {
        (self.checklist_view.is_shown() && !self.checklist.is_empty()).then_some(&self.checklist)
    }

    /// Ctrl+T: show or hide the checklist and record the choice. With no
    /// tasks nothing shows either way; a store that could not be read is
    /// named once.
    fn toggle_checklist(&mut self, fx: &mut Vec<Effect>) {
        self.checklist_view = self.checklist_view.toggled();
        fx.push(Effect::SetChecklistView(self.checklist_view));
        if let Some(why) = self.checklist_note.take() {
            self.transcript.push_notice(
                NoticeLevel::Info,
                format!("the task checklist could not be read: {why}"),
            );
        }
    }

    /// Seed the prompt history (oldest first).
    pub fn with_history(mut self, history: Vec<String>) -> Self {
        self.input = InputEditor::new().with_history(history);
        self
    }

    /// Set the context-compaction policy (default: warn at 75%, repeat at 85%).
    #[must_use]
    pub fn with_compaction_policy(mut self, policy: CompactionPolicy) -> Self {
        self.gauge = CompactionGauge::new(policy);
        self
    }

    /// The context-compaction policy in effect.
    #[cfg(any(test, feature = "test-support"))]
    pub fn compaction_policy(&self) -> CompactionPolicy {
        self.gauge.policy()
    }

    /// Replace the prompt history (another realm's, after a re-home).
    pub fn replace_history(&mut self, history: Vec<String>) {
        self.input.set_history(history);
    }

    /// Seed the global prompt history (oldest first), searched by Ctrl-R after
    /// the workspace history. Up/Down recall stays workspace-only.
    pub fn with_global_history(mut self, history: Vec<String>) -> Self {
        self.global_history = history;
        self
    }

    /// Lines submitted with Enter (prompts and slash commands, as typed) since the
    /// last call, oldest first; blank submissions are never recorded.
    #[cfg(any(test, feature = "test-support"))]
    pub fn take_submitted(&mut self) -> Vec<String> {
        std::mem::take(&mut self.submitted)
    }

    /// Record the transcript viewport height (set by the renderer).
    pub fn set_viewport_height(&mut self, height: usize) {
        self.viewport_height = height.max(1);
    }

    /// Return and reset the redraw flag.
    #[cfg(any(test, feature = "test-support"))]
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Everything the UI loop has to do itself since the last call, in the
    /// order it should: the submitted lines, the copy, the queued
    /// editor/suspend/repaint requests, then the bell and a frame when due.
    pub fn take_local(&mut self) -> Vec<LocalEffect> {
        let mut out: Vec<LocalEffect> = std::mem::take(&mut self.submitted)
            .into_iter()
            .map(LocalEffect::Submitted)
            .collect();
        out.extend(self.clipboard.take().map(LocalEffect::Copy));
        out.append(&mut self.local);
        if std::mem::take(&mut self.ring) {
            out.push(LocalEffect::Bell);
        }
        if std::mem::take(&mut self.dirty) {
            out.push(LocalEffect::Draw);
        }
        out
    }

    /// Process one message. Pure: no I/O, only state changes and returned effects.
    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        self.update_at(msg, Instant::now())
    }

    /// [`Self::update`] with an explicit clock (for tests).
    pub fn update_at(&mut self, msg: Msg, now: Instant) -> Vec<Effect> {
        let mut fx = Vec::new();
        let mut dirty = true;
        match msg {
            Msg::Key(key) => {
                if self.selection.is_some() {
                    self.clear_selection();
                    let ctrl_c = key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL);
                    if (key.code == KeyCode::Esc || ctrl_c) && self.modal.is_none() {
                        // Esc and Ctrl-C only dismiss the selection.
                        self.dirty = true;
                        return fx;
                    }
                }
                if self.modal.is_some() {
                    self.modal_key(&key, now, &mut fx);
                } else {
                    self.main_key(&key, now, &mut fx);
                }
            }
            Msg::Paste(text) => {
                if let Some(Modal::Escalation { dialog, .. }) = &mut self.modal {
                    dialog.type_text(&text);
                } else if self.modal.is_none() {
                    self.chord = None;
                    if let InputMode::Search(search) = &mut self.mode {
                        for c in text.chars().filter(|c| !c.is_control()) {
                            search.push(c, &self.search_corpus);
                        }
                    } else {
                        self.input.insert_str(&text);
                    }
                }
            }
            Msg::Resize => {
                // Logical lines change with the width.
                self.clear_selection();
                self.transcript.invalidate_all();
            }
            Msg::Mouse(m) => dirty = self.mouse(m, now),
            Msg::Agent(env) => {
                self.last_seq = env.seq;
                match &env.payload {
                    AgentEvent::TextDelta { delta, .. } if !delta.is_empty() => {
                        self.streamed_text = true;
                    }
                    AgentEvent::TextComplete { content, .. } if !content.is_empty() => {
                        self.streamed_text = true;
                    }
                    // A turn without provider accounting advances no counter.
                    // Its usage counts for the model the provider accounted
                    // it to (the session's model unless it names none).
                    AgentEvent::TurnCompleted {
                        usage: Some(usage), ..
                    } => {
                        self.status.context_used = Some(context_tokens(usage));
                        self.status.context_approx = false;
                        let accounted = usage.accounting().model.trim();
                        let model = if accounted.is_empty() {
                            self.status.model.clone()
                        } else {
                            accounted.to_string()
                        };
                        self.status.cost.record(&model, usage);
                        fx.push(Effect::CostSnapshot(self.status.cost.clone()));
                    }
                    AgentEvent::CompactionStarted {
                        estimated_history_tokens,
                        ..
                    } => {
                        self.compaction_input = Some(*estimated_history_tokens);
                    }
                    AgentEvent::CompactionCompleted {
                        summary_tokens,
                        estimated_history_tokens_after,
                        summary_source,
                        ..
                    } => {
                        // The history estimate the rewrite left (the
                        // summary alone for a completion that reports none).
                        self.status.context_used =
                            Some(estimated_history_tokens_after.unwrap_or(*summary_tokens));
                        self.status.context_approx = true;
                        self.gauge.compacted();
                        let input = self.compaction_input.take().unwrap_or(0);
                        // Only a provider call costs anything; its input is
                        // the history estimate, its output the summary.
                        if summary_source.as_ref().is_none_or(|source| {
                            matches!(
                                source,
                                meerkat_core::CompactionSummarySource::ProviderCall { .. }
                            )
                        }) {
                            self.status.cost.record_compaction(
                                &self.status.model,
                                input,
                                *summary_tokens,
                            );
                            fx.push(Effect::CostSnapshot(self.status.cost.clone()));
                        }
                    }
                    _ => {}
                }
                if let Some(usage) = self.transcript.apply_agent_event_at(&env.payload, now) {
                    self.status
                        .add_tokens(usage.input_tokens, usage.output_tokens);
                }
            }
            Msg::Scoped(ev) => {
                self.record_helper_usage(&ev, &mut fx);
                self.transcript.apply_scoped_event_at(&ev, now);
            }
            Msg::TurnFinished(outcome) => {
                self.turn_finished(outcome, now, &mut fx);
                // Not when the queued messages started the next turn.
                if !self.running {
                    self.notify();
                }
            }
            Msg::SteerAck { id, result } => self.steer_ack(id, result, now, &mut fx),
            Msg::SteerDelivered(id) => self.pending.show_delivered(&mut self.transcript, id),
            Msg::Recalled { recalled, errors } => self.recalled(&recalled, &errors),
            Msg::QuitDecided(decision) => self.quit_decided(decision, &mut fx),
            Msg::Rerooted {
                workspace,
                realm,
                shell_confined,
            } => {
                self.workspace = Some(workspace);
                self.path_index = PathIndex::Dropped;
                self.path_index_generation += 1;
                self.shell_confined = shell_confined;
                self.local.push(LocalEffect::Reroot(realm));
            }
            Msg::Hangup => self.leave(None, &mut fx),
            Msg::Diff {
                tool_use_id,
                path,
                unified,
                added,
                removed,
            } => {
                let view = DiffView {
                    path: path.clone(),
                    unified,
                    added,
                    removed,
                };
                if !self.transcript.attach_diff(&tool_use_id, view) {
                    self.transcript.push_notice(
                        NoticeLevel::Info,
                        format!("edited {path} (+{added} -{removed})"),
                    );
                }
            }
            Msg::Escalation(prompt, id) => {
                self.escalation(prompt, id);
                self.notify();
            }
            Msg::Focus(focused) => {
                self.focused = focused;
                dirty = false;
            }
            Msg::EscalationResolved(id) => self.escalation_resolved(id),
            Msg::Approval(n) => {
                self.transcript.push(Block::Approval {
                    tool: n.tool,
                    tier: n.tier,
                    allowed: n.allowed,
                    reason: n.reason,
                });
            }
            Msg::ToolNote { tool_use_id, text } => {
                self.transcript.attach_note(&tool_use_id, text);
            }
            Msg::ClassifierUsage { model, usage } => {
                self.status.cost.record(&model, &usage);
                fx.push(Effect::CostSnapshot(self.status.cost.clone()));
            }
            Msg::Notice { level, text } => {
                self.transcript.push_notice(level, text);
            }
            Msg::Status(update) => {
                // Envelope sequence numbers restart with every session task.
                if matches!(update, StatusUpdate::Session(_)) {
                    self.last_seq = 0;
                }
                if let StatusUpdate::Helpers(helpers) = &update {
                    self.transcript.note_live_helpers(
                        helpers
                            .iter()
                            .map(|h| (h.label.as_str(), h.model.as_str(), h.depth)),
                    );
                }
                self.status.apply(update);
            }
            Msg::ShowResumePicker(items) => {
                let picker = Modal::Resume(ResumePicker::new(items));
                match self.modal {
                    None | Some(Modal::Keys) => self.modal = Some(picker),
                    // Shown once the escalations are answered; replaces a
                    // picker parked earlier.
                    Some(Modal::Escalation { .. }) => self.parked = Some(picker),
                    Some(_) => {}
                }
            }
            Msg::ShowModelPicker(groups) => {
                if matches!(self.modal, None | Some(Modal::Keys)) && self.parked.is_none() {
                    self.modal = Some(Modal::Model(ModelPicker::new(groups)));
                } else {
                    for warning in ModelPicker::warnings(&groups) {
                        self.transcript.push_notice(NoticeLevel::Warn, warning);
                    }
                }
            }
            Msg::RemoteTurn { prompt } => self.remote_turn(prompt, now),
            Msg::Peer { from, text } => {
                self.transcript.push(Block::Peer { from, text });
                self.scroll.bottom();
            }
            Msg::Triggered { label, prompt } => {
                self.transcript
                    .label_triggered(self.run_first_block, label, prompt);
                self.scroll.bottom();
            }
            Msg::ResetTranscript(blocks) => {
                self.clear_selection();
                self.panel_selected = None;
                self.helper_view = None;
                self.transcript.reset(blocks);
                self.last_seq = 0;
                // Keep placeholders for messages still in flight.
                self.pending.restore_placeholders(&mut self.transcript);
                if let Some(run) = &mut self.shell_run {
                    run.block = self.transcript.push(Block::Shell {
                        command: run.command.clone(),
                        state: ShellState::Running { since: run.since },
                        output: None,
                        collapsed: false,
                    });
                }
                self.run_first_block = self.transcript.len();
                self.scroll.bottom();
            }
            Msg::CompactRequested(result) => self.compact_requested(result),
            Msg::EditFile { target, path } => {
                self.local
                    .push(LocalEffect::ExternalEditor(EditorTarget::Memory {
                        target,
                        path,
                    }));
            }
            Msg::EditorClosed(closed) => self.editor_closed(closed, &mut fx),
            Msg::ShellFinished { id, outcome } => self.shell_finished(id, outcome, now, &mut fx),
            Msg::Checklist(list) => {
                self.checklist = list;
                self.checklist_note = None;
            }
            Msg::ChecklistUnavailable(why) => self.checklist_note = Some(why),
            Msg::Mentions { attached, sealed } => self.note_mentions(&attached, &sealed),
            Msg::PathIndex { generation, paths } => {
                if self.path_index == PathIndex::Indexing(generation) {
                    self.path_index = PathIndex::Ready(paths);
                    // Rebuild the menu for the word under the cursor.
                    self.menu = None;
                }
            }
            Msg::Tick => {
                let helpers_spun = self.transcript.tick_helpers(now);
                dirty = helpers_spun
                    || self.status.running_since.is_some()
                    || self.status.classifying
                    || self.last_ctrl_c.is_some()
                    || self.quit_confirm.is_some()
                    || self.chord.is_some();
                if self.last_ctrl_c.is_some() && !self.quit_armed(now) {
                    self.last_ctrl_c = None;
                }
                if self.quit_confirm.is_some() && !self.quit_confirm_armed(now) {
                    self.quit_confirm = None;
                }
                if self.chord.is_some() && !self.chord_pending(now) {
                    self.chord = None;
                }
            }
        }
        self.status.queued = self.pending.len();
        self.status.helper_panel = !self.transcript.panel_helpers().is_empty();
        self.refresh_menu();
        self.dirty |= dirty;
        fx
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
