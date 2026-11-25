//! ratatui application: Elm-style state + pure `update`, a renderer and a terminal runtime.
//!
//! [`App::update`] is pure — it never touches the terminal or performs I/O — and
//! returns [`Effect`]s that the client runner (session/approval wiring) executes.
//! Everything the client runner wants to show is pushed back in as a [`Msg`].

mod btw;
pub mod checklist;
pub mod commands;
pub mod compaction;
pub mod complete;
pub mod connection;
mod deferred;
pub mod delivery;
mod dispatch;
pub mod editor;
mod escalations;
pub mod failures;
mod helpers;
pub mod history;
pub mod input;
mod input_mode;
pub mod keymap;
mod keys;
pub mod markdown;
pub mod modal;
mod mouse;
pub mod paste;
pub mod peer;
pub mod permissions;
pub mod picker;
#[cfg(any(test, feature = "test-support"))]
mod probe;
pub mod render;
pub mod runtime;
pub mod search;
pub mod selection;
mod shell;
pub mod transcript;
mod turn_state;
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

pub use crate::cost::SessionCost;
#[cfg(any(test, feature = "test-support"))]
pub use crate::cost::{CostReadout, UsageTotals};
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
pub use delivery::{Pending, PendingId, PendingQueue, PendingState, SteerFailure};
pub use history::History;
pub use input::{InputEditor, StashOutcome};
pub use keymap::{Action, EditAction};
pub use modal::{
    EscalationAnswer, EscalationDialog, EscalationId, HelpDialog, HelpView, Modal, ModalKey,
    ModelPicker, PlanChoice, PlanDialog, QuitChoice, QuitDialog, ResumePicker,
};
pub use permissions::{Section, permissions_dialog};
#[cfg(any(test, feature = "test-support"))]
pub use picker::Does;
pub use picker::PickerLine;
pub use picker::{
    AgentEntry, Field, FieldFor, ListPicker, PickAct, PickKey, PickRow, PickerKind, RuleForm,
};

/// A picker's lines as text, `width` wide (tests of pickers built
/// elsewhere).
#[cfg(test)]
pub(crate) fn picker_lines_for_test(picker: &ListPicker, width: usize) -> Vec<String> {
    picker::list_picker_lines(picker, width, 100)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect()
}
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

/// Presses of Esc this close to each other belong to one interrupt: after
/// its turn ended (its typed messages back in the editor) they do not clear
/// the editor, and the runner says once that the turn is already stopped.
pub const INTERRUPT_REPEAT: Duration = Duration::from_secs(2);

/// How long the user must have been quiet, with a dialog the application
/// opened by itself on screen, before a key answers it ([`Arming`]).
pub const ARM_AFTER: Duration = Duration::from_millis(600);

/// What a dialog the application opened by itself says once it dropped a
/// key ([`Arming`]).
pub const KEYS_IGNORED: &str = "keys are ignored for a moment: you were typing";

/// A dialog that opened by itself while the user may be typing (an
/// approval, the next one queued, "Plan ready"): the next characters of
/// the sentence must not answer it. It is armed once [`ARM_AFTER`] has
/// passed since it opened with no key pressed; each key pressed before
/// that is dropped (not answered, not typed into the input) and starts
/// the wait again. Scrolling and Ctrl+O act at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Arming {
    /// When it opened, or the last key it dropped.
    quiet_since: Instant,
    /// A key was dropped: the dialog says so ([`KEYS_IGNORED`]).
    dropped: bool,
}

impl Arming {
    /// A dialog opened at `now`.
    pub(crate) fn opened(now: Instant) -> Self {
        Self {
            quiet_since: now,
            dropped: false,
        }
    }

    /// Whether it is armed at `now`.
    pub(crate) fn armed(self, now: Instant) -> bool {
        now.saturating_duration_since(self.quiet_since) >= ARM_AFTER
    }

    /// Whether the dialog says that keys are ignored, at `now`.
    pub(crate) fn says_ignored(self, now: Instant) -> bool {
        self.dropped && !self.armed(now)
    }
}

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

use input_mode::{InputMode, Pressed};

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
    /// The user interrupted the turn, and the daemon never confirmed that
    /// its run ended (not even after a hard cancel): it may still be going.
    #[error("the interrupt was not confirmed")]
    Unconfirmed,
}

impl TurnFailure {
    /// How much this end weighs against the turn's other runs' ends
    /// ([`merge_turn_end`]): a failure outranks an interruption, which
    /// outranks a completion (0).
    #[must_use]
    pub fn rank(end: &Result<TurnSummary, Self>) -> u8 {
        match end {
            Ok(_) => 0,
            Err(Self::Interrupted | Self::Unconfirmed) => 1,
            Err(Self::Failed(_)) => 2,
        }
    }
}

/// Fold one run's end `next` into the turn's end so far, :
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

/// Status-bar update pushed by the client runner.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusUpdate {
    /// The endpoint and model changed.
    Model {
        /// The endpoint's name.
        endpoint: String,
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
        denials: crate::approval::desk::Denials,
    },
    /// Whether the classifier is currently running.
    Classifying(bool),
    /// A classifier check (`/auto check`) has judged this many of its
    /// cases (`None`: none runs).
    ClassifierCheck(Option<(usize, usize)>),
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
    /// The session's token totals by model, as the daemon counted them
    /// (on attach, after a switch, and pushed after each model call).
    CostTotals(SessionCost),
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
    /// The turn started by [`Effect::StartTurn`] finished (with every
    /// follow-up run the runtime added for steered messages): completed,
    /// or how it ended without a result. The messages it did not deliver
    /// are the UI's own to find ([`delivery::PendingQueue::take_unsent`]).
    TurnFinished(Result<TurnSummary, TurnFailure>),
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
    /// The user's interrupt took these messages back (oldest first): the
    /// agent never reads them, and they return to the editor. The turn's
    /// end says how many came back, in its one line (or, after the turn,
    /// one line of its own).
    Returned(Vec<PendingId>),
    /// The answer to the pending `/btw` ([`Effect::Btw`]), or why there is
    /// none, in plain words.
    BtwAnswered(Result<String, String>),
    /// A steered message the runtime ended without writing it into a run
    /// (abandoned: the run was stopped, the agent retired, …): the agent
    /// never reads it, and it returns to the editor as a recalled one does.
    NotDelivered {
        /// The message.
        id: PendingId,
        /// Why, in words, when the runtime recorded why.
        why: Option<String>,
    },
    /// A steered message shown as delivered whose request the daemon's
    /// stop cut off: the engine does not keep it, so it returns to the
    /// editor.
    Unkept {
        /// What it said.
        text: String,
        /// Why, in words.
        why: String,
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
    /// A notice to show in the transcript.
    Notice {
        /// Severity.
        level: NoticeLevel,
        /// Text.
        text: String,
    },
    /// Status-bar update.
    Status(StatusUpdate),
    /// A `[ui]` setting in force changed (`rkat/setting`): the key and its
    /// value as the file holds it (`None`: the default).
    Setting {
        /// The dotted key.
        key: String,
        /// The value.
        value: Option<String>,
    },
    /// Open the resume picker with `(ref, label)` items.
    ShowResumePicker(Vec<(String, String)>),
    /// Open the model picker (answer to [`Effect::RequestModelPicker`]): one
    /// group per endpoint, the current endpoint and model first; a group's
    /// warning says why its list is missing.
    ShowModelPicker(Vec<ModelGroup>),
    /// Open the model picker for the classifier (`/auto model`): the same
    /// lists, titled "Classifier model", the classifier's model first.
    ShowClassifierPicker(Vec<ModelGroup>),
    /// Open the agent picker (answer to [`Effect::Agents`] without a
    /// name): the workspace's agents, names in order.
    ShowAgentPicker(Vec<AgentEntry>),
    /// The agents again after a retire from the picker: an open agent
    /// picker shows them, the highlight on the row where it was (the next
    /// agent, or the last when the retired one was last); nothing opens.
    RefreshAgentPicker(Vec<AgentEntry>),
    /// Why a retire from the picker was refused: an open agent picker
    /// shows it.
    AgentPickerRefused(String),
    /// Why a picker's act was refused by the daemon: a picker of `kind`
    /// open or waiting shows it under its rows.
    PickerRefused {
        /// Whose picker.
        kind: PickerKind,
        /// Why.
        why: String,
    },
    /// What a picker's act did ("approved once: …"): a picker of `kind`
    /// open or waiting states it under its rows (after the refreshed list).
    PickerDone {
        /// Whose picker.
        kind: PickerKind,
        /// What happened.
        text: String,
    },
    /// Show a list picker (`/schedules`, `/permissions`, `/memory`). A
    /// `refresh` is the list again after the picker's own act: it replaces
    /// only a picker of its kind still open (or waiting behind an approval
    /// dialog).
    ShowPicker {
        /// The picker.
        picker: ListPicker,
        /// Replace an open picker of its kind only.
        refresh: bool,
    },
    /// The runner's check of the directory typed in the `/rehome`
    /// picker ([`Effect::RehomeCheck`]): the question to ask and the
    /// directory (canonical) it moves this workspace's state to, or why
    /// not, shown in the field.
    RehomeChecked {
        /// The text checked.
        typed: String,
        /// The question and the directory, or the refusal.
        answer: Result<(String, std::path::PathBuf), String>,
    },
    /// Replace the transcript (e.g. after resuming a session).
    ResetTranscript(Vec<Block>),
    /// A turn this UI did not start runs on the daemon (another
    /// client's, or one the catch-up found in progress): show `prompt` as
    /// the user's message when there is one and count the UI busy until
    /// [`Msg::TurnFinished`], as if it had started it. During a running
    /// turn: another client's steer joined it as a follow-up run, shown as
    /// the user's message.
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
        /// A note of helpers' endings: its folded line as the daemon made
        /// it from the endings (`None`: read from the note's header).
        summary: Option<crate::daemon::agents::endings::NoteSummary>,
    },
    /// The daemon listed the workspace's mob: these members are seated in
    /// it (a helper of the agent's own mob it does not name has ended).
    SeatedHelpers(Vec<String>),
    /// A helper of this conversation ended: its line says how, and a notice
    /// says who was told and when.
    HelperEnded {
        /// Its labels, outermost first.
        path: Vec<String>,
        /// How it ended (`stopped by you`).
        how: String,
        /// Who was told and when, when anyone was.
        told: Option<String>,
    },
    /// `/compact now`: the engine took the request, so the session's next
    /// compaction check compacts (or why it did not). The
    /// `CompactionStarted`/`Completed` events confirm the compaction itself;
    /// `RequestedCompactionSkipped` says why nothing was compacted.
    CompactRequested(Result<(), CompactFailure>),
    /// The run `/compact` starts on an idle agent began (`true`) or ended:
    /// while it runs, a message typed is held and sent after it.
    CompactionRun(bool),
    /// The summary of the latest compaction, read from the conversation's
    /// record ([`Effect::ReadCompactionSummary`]), or why it could not be.
    CompactionSummary(Result<String, String>),
    /// The terminal gained (`true`) or lost (`false`) the focus.
    Focus(bool),
    /// The client runner resolved `/memory`'s file (and created a missing
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
    /// mentions complete there, and the input history and drafts are the
    /// realm's at `realm`.
    Rerooted {
        /// The workspace.
        workspace: ProjectRoot,
        /// Its realm directory.
        realm: std::path::PathBuf,
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

/// Notice shown once the engine took a `/compact` request while a turn
/// of the agent runs (another client's): that turn compacts at its next
/// model request.
pub const COMPACT_REQUESTED: &str = "the context will be compacted before the agent's next model request (the ✓ compacted line confirms it)";

/// The prompt of the run `/compact` starts on an idle agent so that its
/// compaction check runs (the client runner's `compact_asked`): it stays in
/// the conversation (the engine keeps a run's input), costs one short model
/// call after the summary, and is left out wherever rkat-tui shows the
/// conversation (live and restored).
pub const COMPACT_PROMPT: &str = "[rkat-tui /compact] The user asked to compact this conversation; it was compacted before this message. Reply with the single word: compacted";

/// Notice shown once the engine took a `/compact` request on the idle
/// agent: the client starts the short run whose check compacts.
pub const COMPACT_RUNNING: &str = "compacting now: the agent makes one short request after the summary (the ✓ compacted line confirms it; what you send meanwhile goes after it)";

/// Side effect requested by [`App::update`], executed by the client runner.
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
    /// Interrupt the running turn. After Ctrl-X Ctrl-S the UI sends the
    /// waiting messages the interrupt hands back and the draft as the next
    /// turn itself once [`Msg::TurnFinished`] arrives.
    Interrupt,
    /// Pull back every steered message the runtime still holds queued (not
    /// yet joined to the run) without touching the run; answered with
    /// [`Msg::Recalled`].
    RecallQueued,
    /// Steer a message into the running turn (a durable `Steer` input);
    /// answered with [`Msg::SteerAck`], then [`Msg::SteerDelivered`] or,
    /// when the runtime ended it without a run, [`Msg::NotDelivered`].
    Steer {
        /// Queue id allocated by the UI.
        id: PendingId,
        /// Raw user text, sent as an ordinary user message.
        text: String,
    },
    /// Leave the application. The client runner interrupts a running turn,
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
    /// client runner saves those the model never received.
    SaveUnsent(Vec<(Option<PendingId>, String)>),
    /// Start a fresh session.
    NewSession,
    /// Resume a session by reference (`last`, `~N`, id prefix).
    ResumeSession(String),
    /// Load session list; answer with [`Msg::ShowResumePicker`].
    RequestResumePicker,
    /// `/set`, `/unset` and their shorthands (`/compact prepare`, `/mode
    /// <mode> --user`): each `(key, value)` in order (`rkat/set_setting`;
    /// `None` removes the level's value), at the user's level (`user`) or
    /// the workspace's; each answer a notice.
    SetSettings {
        /// The changes.
        changes: Vec<(String, Option<String>)>,
        /// The user's own `tui.toml`.
        user: bool,
    },
    /// `/settings [filter]`: open the settings picker (answered with
    /// [`Msg::ShowPicker`]).
    RequestSettingsPicker(Option<String>),
    /// The settings picker's change: as [`Self::SetSettings`] of one
    /// change, then the picker's list again.
    PickedSetting {
        /// The key.
        key: String,
        /// The value (`None`: remove it).
        value: Option<String>,
        /// The user's own `tui.toml`.
        user: bool,
        /// The filter the picker was opened with (its list comes again
        /// with it).
        filter: Option<String>,
    },
    /// `/compact status`: the daemon's side of it, the prepare instruction
    /// (`rkat/compact_status`), as a notice.
    CompactPrepareStatus,
    /// `/compact prepare`: the same, then the forms that change it.
    CompactPrepareShow,
    /// Leaving with commands still held for the turn's end: they are
    /// dropped; the runner says so after the UI closes too.
    DroppedCommands(Vec<String>),
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
        /// Written to the user's own `tui.toml`.
        user: bool,
    },
    /// Remove rule number `n`.
    RemoveRule(u64),
    /// List rules; answer with [`Msg::Notice`].
    ListRules,
    /// `/permissions`: open its dialog on the section given, else on
    /// Blocked when a call is blocked or an approval waits, else on In
    /// force (answered with [`Msg::ShowPicker`]).
    RequestPermissions(Option<Section>),
    /// The `/permissions` dialog's confirmed removal of rule `n`: as
    /// `/permissions rm <n>`, then the dialog's lists again.
    PickedRuleRemoval(u64),
    /// The dialog's confirmed removal of session grant `id`, then its
    /// lists again.
    PickedGrantRemoval(u64),
    /// The dialog's confirmed removal of statement `id`, then its lists
    /// again.
    PickedStatementRemoval(u64),
    /// The dialog's `n`: allow the kind of action `text` describes for
    /// the scope, then its lists again.
    PickedStatement {
        /// This session, always for this workspace or in every workspace.
        scope: crate::approval::grants::GrantScope,
        /// The statement as typed.
        text: String,
    },
    /// The dialog's `r`: add the rule, then its lists again.
    PickedRule(RuleForm),
    /// The dialog's `e` on a rule: replace rule `n`, which read `old`,
    /// with `new` in place, then its lists again (a refusal shows in the
    /// open field).
    PickedRuleEdit {
        /// The rule's number.
        n: u64,
        /// The rule as the dialog showed it.
        old: crate::approval::desk::RuleSpec,
        /// The rule as edited.
        new: crate::approval::desk::RuleSpec,
    },
    /// `/auto stats`: the review numbers, as a notice.
    AutoStats,
    /// `/auto check`: measure the classifier model on these corpus cases
    /// (the daemon's `rkat/classifier_check`), the result as a notice.
    ClassifierCheck(crate::approval::corpus::Selection),
    /// The `/permissions` dialog's Blocked choice: approve blocked call
    /// `id` for the scope (with a statement: allow that kind of action
    /// instead), then the dialog's lists again.
    PickedApproval(u64, crate::approval::grants::GrantScope, Option<String>),
    /// The Blocked section's `w`: withdraw the approval once of call
    /// `id`, then the dialog's lists again.
    PickedWithdrawal(u64),
    /// The Hosts section's `n`: trust the host typed, in this workspace
    /// or (`user`) every workspace, then the dialog's lists again.
    PickedHostTrust {
        /// As typed.
        host: String,
        /// The user's own `tui.toml`.
        user: bool,
    },
    /// The Blocked section's `t`/`T`: trust the hosts blocked call `id`'s
    /// D5 block names, then the dialog's lists again.
    PickedCallHostTrust {
        /// The blocked call.
        id: u64,
        /// The user's own `tui.toml`.
        user: bool,
    },
    /// The Hosts section's `d`: stop trusting the host, then the dialog's
    /// lists again.
    PickedHostRemoval(String),
    /// `/trust`: record every project input's digest
    /// in the host trust record; their servers and hooks run from the next
    /// session.
    TrustProject,
    /// Switch the permission mode (`Shift+Tab`, `/mode`, `/auto on|off`).
    SetMode(PermissionMode),
    /// Record the checklist panel's view (Ctrl+T) as `[ui] checklist` in
    /// the realm `tui.toml`.
    SetChecklistView(ChecklistView),
    /// Switch model.
    SetModel(String),
    /// `/auto model`: open the model picker for the classifier (answered
    /// with [`Msg::ShowClassifierPicker`]).
    RequestClassifierPicker,
    /// `/auto model <ref> [--user]` or the classifier picker's choice:
    /// the classifier reviews with that model (`rkat/set_setting` of
    /// `auto.classifier_model`, at the user's level with `true`).
    SetClassifierModel(String, bool),
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
    /// `/context`: the UI's part of the report (the gauge and the
    /// compaction policy); the client runner adds what the context holds and
    /// answers with one [`Msg::Notice`].
    ContextReport(String),
    /// A compaction completed: read its summary from the conversation's
    /// record (`rkat/transcript`; the event carries only its figures) and
    /// answer with [`Msg::CompactionSummary`].
    ReadCompactionSummary,
    /// `/btw`: ask the daemon the side question `question` (`rkat/btw`),
    /// with the tool the agent is running and for how long; answered with
    /// [`Msg::BtwAnswered`].
    Btw {
        /// The question.
        question: String,
        /// The running tool's name and its seconds so far.
        running_tool: Option<(String, u64)>,
        /// The running turn as this client shows it (its stored history
        /// has the run only once it ended).
        current_turn: Option<String>,
    },
    /// Esc on a pending `/btw`: cancel it (`rkat/btw_cancel`); the agent's
    /// run is not touched.
    CancelBtw,
    /// `/export [file]`: write `text` (the transcript as plain text) to
    /// `path` (relative to the workspace), or to a fresh file in the
    /// workspace's realm when `None`; answered with a notice.
    Export {
        /// Where to write, as typed.
        path: Option<String>,
        /// The transcript text.
        text: String,
    },
    /// `/status`: the client runner answers with one [`Msg::Notice`] naming
    /// the version, model, session, workspace, realm, confinement, auto
    /// mode, trust state and helper depth limit.
    StatusReport,
    /// `/daemon stop`: ask the attached daemon to shut down (
    /// answered with a notice).
    DaemonStop,
    /// `/rehome`: open the picker of this workspace's state (`None`,
    /// answered with [`Msg::ShowPicker`]), or re-home: the runner stops
    /// the daemon, moves the realm, re-roots itself and reattaches,
    /// telling the UI with [`Msg::Rerooted`].
    Rehome(Option<String>),
    /// The `/rehome` picker's directory as typed: check it as `/rehome to`
    /// reads it (answered with [`Msg::RehomeChecked`]); nothing changes.
    RehomeCheck(String),
    /// `/rehome <argument>` again after the user confirmed deleting the
    /// state `discard` (a realm id) that refused it: replaced whatever it
    /// holds (a turn or another client there still refuses).
    RehomeDiscarding {
        /// The `/rehome` argument.
        argument: String,
        /// The realm to delete.
        discard: String,
    },
    /// The `/rehome` picker's confirmed `d`: delete realm `id` (as
    /// `rkat-tui --remove-state <id> --yes`), then the picker again.
    RemoveState(String),
    /// `/daemon start`: connect to the workspace's daemon again (starting
    /// it when this client may) after the connect failed or the daemon
    /// stopped (answered with notices and the connection's state).
    DaemonStart,
    /// `/agents`: open the agent picker (`None`, answered with
    /// [`Msg::ShowAgentPicker`]), or switch this client to the agent named
    /// (answered with notices).
    Agents(Option<String>),
    /// `/wire`: open the picker of this agent's wires and the other
    /// workspaces' agents (`None`, answered with [`Msg::ShowPicker`]), or
    /// wire this agent to the one named (answered with a notice).
    Wire(Option<String>),
    /// The `/wire` picker's choice: wire this agent to `agent` of
    /// `workspace` (answered with a notice).
    WireTo {
        /// The other agent's workspace.
        workspace: std::path::PathBuf,
        /// The other agent.
        agent: String,
    },
    /// The `/wire` picker's confirmed `d` on a wired row: remove this
    /// agent's wire to `agent` of `workspace`, on both sides when the
    /// other daemon runs (answered with a notice; the picker refreshes).
    RemoveWire {
        /// The other agent's workspace.
        workspace: std::path::PathBuf,
        /// The other agent.
        agent: String,
    },
    /// The agent picker's "new agent" row: seat a fresh agent for this
    /// client and switch to it (answered with notices).
    NewAgent,
    /// The agent picker's confirmed retire: retire another agent that is
    /// idle and has no client (`rkat/retire_agent`; answered with a
    /// notice).
    RetireAgent(String),
    /// Stop the helper with these labels (outermost first), confirmed in
    /// its full view or the `/helpers` picker (`rkat/stop_helper`; a
    /// refusal answers with a notice, the ending with
    /// [`Msg::HelperEnded`]).
    StopHelper(Vec<String>),
    /// `/audit [n]`: show the daemon's audit log's last lines for this
    /// client's agent (answered with a notice).
    Audit(Option<usize>),
    /// `/schedules`: open the schedule picker (`None`, answered with
    /// [`Msg::ShowPicker`]), or act on one (answered with a notice).
    Schedules(Option<(crate::daemon::protocol::ScheduleOp, String)>),
    /// The schedule picker's act on the schedule with this id: as
    /// `/schedules <op> <id>`, then the picker's list again.
    PickedSchedule(crate::daemon::protocol::ScheduleOp, String),
    /// `/memory`: open the instruction-file picker (answered with
    /// [`Msg::ShowPicker`]).
    RequestMemoryPicker,
    /// `/memory <file>`: resolve the instruction file (creating `SYSTEM.md` with
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
/// reaches the client runner. Drained with [`App::take_local`].
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
    /// The endpoint's name.
    endpoint: String,
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
    /// The classifier's blocks and their limits.
    denials: crate::approval::desk::Denials,
    /// When the running turn started.
    running_since: Option<Instant>,
    /// Number of messages typed mid-turn that have not been delivered yet.
    queued: usize,
    /// Whether one of the user's messages waits for the running turn, so
    /// the queued segment says that Ctrl-X Ctrl-S pushes it.
    send_now: bool,
    /// When the agent's model request in flight started (`None`: none is):
    /// a steer joins only once the reply is in, so the bar says what the
    /// queued messages wait for.
    model_since: Option<Instant>,
    /// When the compaction running now started (`None`: none runs).
    compacting_since: Option<Instant>,
    /// Number of helpers running now.
    helpers: usize,
    /// Whether the helper panel lists any helper (the status bar names the
    /// key that selects them).
    helper_panel: bool,
    /// Whether the classifier is running.
    classifying: bool,
    /// When the classifier review running now started.
    classifying_since: Option<Instant>,
    /// When this client last heard from the agent: any event of its run,
    /// of its helpers, of the classifier or of a compaction. While the
    /// agent is busy, a long silence turns the prompt marker into its
    /// waiting form ([`render::QUIET_AFTER`]).
    heard_at: Option<Instant>,
    /// The session's own tool calls requested and not completed, oldest
    /// first.
    open_tools: Vec<OpenTool>,
    /// When the approval dialog waiting for the user's answer opened.
    asking_since: Option<Instant>,
    /// A classifier check's `(done, total)`, while one runs.
    check: Option<(usize, usize)>,
    /// When the pending `/btw` was asked.
    btw_since: Option<Instant>,
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

/// A tool call of the session the model asked for whose execution has
/// not completed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenTool {
    id: String,
    name: String,
    since: Instant,
}

/// The prompt the "Plan ready" dialog's proceed choices send.
pub const PROCEED_PROMPT: &str = "Proceed with the plan";

impl StatusInfo {
    /// Whether this client reached the daemon ([`Connection::reached`]).
    #[must_use]
    pub fn reached_daemon(&self) -> bool {
        self.connection.as_ref().is_some_and(Connection::reached)
    }

    /// Whether the agent this client works with is busy, as the prompt
    /// marker and the status bar show it: a turn (this client's or one the
    /// daemon reports), a classifier review or a compaction.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.running_since.is_some() || self.classifying || self.compacting_since.is_some()
    }

    /// Follow the session's tool calls from its events: a request opens
    /// one, its completion (or time-out, or result) closes it; the next
    /// model call and the run's end close them all.
    fn track_tools(&mut self, event: &AgentEvent, now: Instant) {
        match event {
            AgentEvent::ToolCallRequested { id, name, .. } => {
                if !self.open_tools.iter().any(|t| t.id == *id) {
                    self.open_tools.push(OpenTool {
                        id: id.clone(),
                        name: name.clone(),
                        since: now,
                    });
                }
            }
            AgentEvent::ToolExecutionCompleted { id, .. }
            | AgentEvent::ToolExecutionTimedOut { id, .. }
            | AgentEvent::ToolResultReceived { id, .. } => {
                self.open_tools.retain(|t| t.id != *id);
            }
            AgentEvent::HookDenied { payload, .. } => {
                if let Some(id) = payload
                    .as_ref()
                    .and_then(|p| p.get("tool_use_id"))
                    .and_then(serde_json::Value::as_str)
                {
                    self.open_tools.retain(|t| t.id != id);
                }
            }
            AgentEvent::TurnStarted { .. }
            | AgentEvent::RunCompleted { .. }
            | AgentEvent::RunFailed { .. } => self.open_tools.clear(),
            _ => {}
        }
    }

    /// Add to the token counters (a model call's usage).
    fn add_tokens(&mut self, input: u64, output: u64) {
        self.input_tokens = self.input_tokens.saturating_add(input);
        self.output_tokens = self.output_tokens.saturating_add(output);
    }

    /// The mode as `/auto` and `/mode` describe it.
    #[must_use]
    pub fn mode_text(&self) -> String {
        match self.mode {
            None => "unknown".to_string(),
            Some(PermissionMode::Auto) => format!(
                "auto{} ({})",
                if self.auto_paused { ", PAUSED" } else { "" },
                self.denials.describe()
            ),
            Some(mode) => mode.label().to_string(),
        }
    }

    /// Apply an client runner update: the only way in from outside the UI.
    pub fn apply(&mut self, update: StatusUpdate) {
        match update {
            StatusUpdate::Model { endpoint, model } => {
                self.endpoint = endpoint;
                self.model = model;
            }
            StatusUpdate::Session(id) => {
                self.session_short = crate::text::short_id(&id).to_string();
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
            StatusUpdate::ClassifierCheck(check) => self.check = check,
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

/// An open helper full view: the helper's header and the view's own
/// scroll position (it follows new output while at the bottom).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpenHelperView {
    header: BlockId,
    scroll: ScrollState,
    /// Esc asked whether to stop the (running) helper.
    confirm_stop: bool,
}

/// Transcript scroll position.
///
/// At the bottom the view follows new output. Scrolled up, it is held by
/// its top line: the first visible line of the last frame ([`Self::top`]),
/// and for the transcript the block and the line within it that line
/// belongs to ([`Self::anchor`], resolved again each frame by the
/// renderer), so output arriving below, a block above that grows or
/// folds, a change of the viewport's height and lines appended to the
/// block the view is inside leave the text on screen where it is. Only
/// the user's own moves (the scroll keys, the wheel, a jump, a fold kept
/// at the bottom) move it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollState {
    /// Lines between the bottom of the viewport and the end of the transcript.
    pub offset_from_bottom: usize,
    /// Whether the view sticks to the bottom as output arrives.
    pub follow: bool,
    last_total: usize,
    /// The view's first line as of the last frame, while scrolled up.
    top: Option<usize>,
    /// The block the view's first line belongs to, and that line's
    /// distance from the block's first line (negative: the block starts
    /// that far below the view's top), while scrolled up.
    anchor: Option<(BlockId, isize)>,
    /// Lines that arrived since the view stopped following.
    unread: usize,
    /// The next frame's change of length is the user's (a fold), not new
    /// output.
    refolded: bool,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            offset_from_bottom: 0,
            follow: true,
            last_total: 0,
            top: None,
            anchor: None,
            unread: 0,
            refolded: false,
        }
    }
}

impl ScrollState {
    /// The user moved the view: what held it lets go.
    fn moved(&mut self) {
        self.top = None;
        self.anchor = None;
    }

    /// Scroll towards older output.
    pub fn up(&mut self, lines: usize) {
        self.moved();
        self.offset_from_bottom = self.offset_from_bottom.saturating_add(lines);
        self.follow = false;
    }

    /// Scroll towards newer output; reaching the bottom re-enables follow.
    pub fn down(&mut self, lines: usize) {
        self.moved();
        self.offset_from_bottom = self.offset_from_bottom.saturating_sub(lines);
        if self.offset_from_bottom == 0 {
            self.bottom();
        }
    }

    /// Jump to the oldest output.
    pub fn top(&mut self) {
        self.moved();
        self.offset_from_bottom = usize::MAX;
        self.follow = false;
    }

    /// Jump to the newest output and follow.
    pub fn bottom(&mut self) {
        self.moved();
        self.offset_from_bottom = 0;
        self.follow = true;
        self.unread = 0;
    }

    /// Show laid-out line `bottom` at the bottom of the window over
    /// `total` lines (as far as the transcript's start allows:
    /// [`Self::window`] bounds it): a fold the user made.
    pub fn anchor_bottom(&mut self, total: usize, bottom: usize) {
        self.moved();
        self.offset_from_bottom = total.saturating_sub(bottom.saturating_add(1));
        self.last_total = total;
        self.refolded = true;
        self.follow = self.offset_from_bottom == 0;
    }

    /// The view's first line as of the last frame (`None`: following, or
    /// moved by the user since).
    #[must_use]
    pub fn held_top(&self) -> Option<usize> {
        self.top
    }

    /// The block (and the line within it) the view is held by.
    #[must_use]
    pub fn anchor(&self) -> Option<(BlockId, isize)> {
        self.anchor
    }

    /// The view's first line is `top` now (its anchor resolved again over
    /// the transcript as laid out for this frame).
    pub fn hold_top(&mut self, top: usize) {
        if !self.follow {
            self.top = Some(top);
        }
    }

    /// After a frame: hold the view by `anchor` (the block of its first
    /// line, and that line's distance from the block's first line).
    pub fn set_anchor(&mut self, anchor: Option<(BlockId, isize)>) {
        self.anchor = if self.follow { None } else { anchor };
    }

    /// How many lines arrived below the view since it stopped following
    /// (0 while following).
    #[must_use]
    pub fn unread(&self) -> usize {
        if self.follow { 0 } else { self.unread }
    }

    /// Reconcile with the laid-out transcript and return the first visible line.
    ///
    /// Following, the last `height` lines; scrolled up, the line it is
    /// held by stays the first ([`Self::held_top`]); moved by the user
    /// since the last frame, as far from the bottom as the move put it.
    pub fn window(&mut self, total: usize, height: usize) -> usize {
        let max_top = total.saturating_sub(height);
        let grown = total.saturating_sub(self.last_total);
        if self.follow {
            self.offset_from_bottom = 0;
        } else if let Some(top) = self.top {
            self.offset_from_bottom = max_top.saturating_sub(top);
        } else if grown > 0 && self.last_total > 0 && !self.refolded {
            self.offset_from_bottom = self.offset_from_bottom.saturating_add(grown);
        }
        if !self.follow && self.last_total > 0 && !self.refolded {
            self.unread = self.unread.saturating_add(grown);
        }
        self.refolded = false;
        self.last_total = total;
        self.offset_from_bottom = self.offset_from_bottom.min(max_top);
        if self.offset_from_bottom == 0 {
            self.follow = true;
            self.unread = 0;
            self.moved();
        }
        let start = max_top - self.offset_from_bottom;
        if !self.follow {
            self.top = Some(start);
        }
        start
    }
}

/// The transcript line a fold change keeps at the bottom of the screen
/// ([`App::keep_bottom`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FoldAnchor {
    /// The block the bottom line belonged to.
    pub block: BlockId,
    /// How many lines of the block came after it.
    pub from_end: usize,
    /// The block being folded, whose last line is the bottom when `block`
    /// is no longer shown.
    pub folded: Option<BlockId>,
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
/// client runner's job — seed recall with [`App::with_history`] (e.g. from
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
    /// What each held [`PendingState::Command`] item does once the agent
    /// is idle ([`deferred`]).
    deferred: std::collections::HashMap<PendingId, deferred::Deferred>,
    /// The agent's turn as this client shows it ([`turn_state::Turn`]).
    turn: turn_state::Turn,
    /// What the user typed beside the pending queue
    /// ([`turn_state::Delivery`]).
    delivery: turn_state::Delivery,
    /// A compaction of the agent's context ([`turn_state::Compaction`]).
    compaction: turn_state::Compaction,
    /// Sequence number of the last primary-stream event processed.
    last_seq: u64,
    /// How the main input takes the next key: editing, an armed Esc or
    /// Ctrl-C, a pending chord, or a history search ([`InputMode`]).
    mode: InputMode,
    /// Context-gauge policy: warn about a filling context (warn mode).
    gauge: CompactionGauge,
    /// Global (cross-workspace) prompt history, oldest first; searched after the
    /// workspace history.
    global_history: Vec<String>,
    pending_escalations: VecDeque<(EscalationId, RecordedPrompt)>,
    /// A picker ([`Modal::Model`], [`Modal::Resume`] or [`Modal::List`]) an escalation
    /// displaced; restored once no escalation remains.
    parked: Option<Modal>,
    /// The dialog the application opened by itself (an approval, the
    /// next one queued, "Plan ready", a picker an approval had displaced)
    /// takes no answer yet ([`Arming`]).
    arming: Option<Arming>,
    submitted: Vec<String>,
    run_first_block: BlockId,
    /// What the current run's failures said, and who started it.
    failures: failures::RunFailures,
    /// The last Esc (or Ctrl+C) that interrupted, while it belongs to that
    /// interrupt ([`INTERRUPT_REPEAT`]).
    last_interrupt: Option<Instant>,
    /// The pending `/btw`'s block and question (one at a time).
    btw: Option<(BlockId, String)>,
    viewport_height: usize,
    /// Text to put on the system clipboard (drained by the runtime).
    clipboard: Option<QueuedCopy>,
    /// What this client copied last (a selection or `/copy`): what a
    /// middle-click pastes ([`App::middle_paste`]). The terminal's own
    /// selection cannot be read back (an OSC 52 query is refused or asks
    /// the user), so this is rkat-tui's own.
    last_copied: Option<String>,
    /// The loop's own effects queued in order ([`LocalEffect::ExternalEditor`],
    /// [`LocalEffect::Suspend`], [`LocalEffect::Redraw`]); the others are
    /// kept in their own fields and coalesced.
    local: Vec<LocalEffect>,
    /// The helper header selected in the helper panel (Ctrl+Up/Down; Enter
    /// on an empty input opens its full view).
    panel_selected: Option<BlockId>,
    /// The helper panel's rows at the last update (a selection whose row
    /// leaves moves to its neighbour, [`Self::settle_panel`]).
    panel_rows: Vec<BlockId>,
    /// `/helpers` lists the ended helpers too (its `a`).
    helpers_all: bool,
    /// The helper whose full view replaces the transcript, if one is open.
    helper_view: Option<OpenHelperView>,
    /// Ring the bell when a turn ends or an approval prompt opens while the
    /// terminal is unfocused (`[ui] bell`).
    bell: bool,
    /// Whether the terminal has the focus (assumed at start-up).
    focused: bool,
    /// A bell is due ([`Self::take_bell`]).
    ring: bool,
    /// The workspace `@` mentions complete and resolve against (`None`:
    /// no path menu, no mention notices).
    workspace: Option<ProjectRoot>,
    /// Ctrl-C, Ctrl-D and `/quit` ([`keys::QuitState`]).
    quit: keys::QuitState,
    /// The mouse: hit-testing and the selection ([`mouse::MouseState`]).
    mouse: mouse::MouseState,
    /// The completion menu and the `@` path index ([`keys::Completion`]).
    completion: keys::Completion,
    /// The `!` commands ([`shell::ShellRuns`]).
    shell: shell::ShellRuns,
    /// The checklist panel ([`checklist::ChecklistPanel`]).
    checklist: checklist::ChecklistPanel,
    /// The line at the bottom of the screen when something was expanded
    /// or folded ([`Self::keep_bottom`]): the next frame puts it back
    /// there ([`ScrollState::anchor_bottom`]).
    fold_anchor: Option<FoldAnchor>,
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
            deferred: std::collections::HashMap::new(),
            turn: turn_state::Turn::default(),
            delivery: turn_state::Delivery::default(),
            compaction: turn_state::Compaction::default(),
            last_seq: 0,
            mode: InputMode::Normal,
            gauge: CompactionGauge::default(),
            global_history: Vec::new(),
            pending_escalations: VecDeque::new(),
            parked: None,
            arming: None,
            submitted: Vec::new(),
            run_first_block: BlockId(0),
            failures: failures::RunFailures::default(),
            last_interrupt: None,
            btw: None,
            viewport_height: 20,
            clipboard: None,
            last_copied: None,
            local: Vec::new(),
            panel_selected: None,
            panel_rows: Vec::new(),
            helpers_all: false,
            helper_view: None,
            bell: true,
            focused: true,
            ring: false,
            workspace: None,
            quit: keys::QuitState::default(),
            mouse: mouse::MouseState::default(),
            completion: keys::Completion::default(),
            shell: shell::ShellRuns::default(),
            checklist: checklist::ChecklistPanel::default(),
            fold_anchor: None,
            dirty: true,
        }
    }

    /// The workspace `@` mentions complete against (the path menu).
    #[must_use]
    pub fn with_workspace(mut self, root: ProjectRoot) -> Self {
        self.transcript
            .set_paths(Some(root.as_path().to_path_buf()), dirs::home_dir());
        self.workspace = Some(root);
        self
    }

    /// Ctrl-O: every foldable block shown in full, or all folded back
    /// ([`Transcript::toggle_detail`]). The line at the bottom of the
    /// screen stays there ([`Self::keep_bottom`]).
    pub(super) fn toggle_detail(&mut self, view: Option<BlockId>) {
        if view.is_none() {
            self.keep_bottom(None);
        }
        self.transcript.toggle_detail();
    }

    /// Call before anything is expanded or folded in the transcript: the
    /// line now at the bottom of the screen is at the bottom of the next
    /// frame too, so what is above it grows or shrinks upwards and folding
    /// again brings back the view as it was. The line is remembered by its
    /// distance from the end of its block, so a block at the bottom opens
    /// upwards too and what it shows is on screen. When the block has
    /// fewer lines after the change, its first line takes the place; when
    /// it is no longer shown at all, the last line of `folded` (the block
    /// being folded) does.
    pub(super) fn keep_bottom(&mut self, folded: Option<BlockId>) {
        if let Some((block, line)) = self.mouse.transcript_rows.last().copied()
            && let Some((start, len)) = self.transcript.line_span(block)
        {
            let offset = line.saturating_sub(start);
            self.fold_anchor = Some(FoldAnchor {
                block,
                from_end: len.saturating_sub(offset + 1),
                folded,
            });
        }
    }

    /// The anchor a fold change left for the next frame (taken).
    pub(crate) fn take_fold_anchor(&mut self) -> Option<FoldAnchor> {
        self.fold_anchor.take()
    }

    /// Whether to ring the bell when a turn ends or an approval prompt opens
    /// while the terminal is unfocused (default on).
    #[must_use]
    pub fn with_bell(mut self, bell: bool) -> Self {
        self.bell = bell;
        self
    }

    /// Whether something animates or waits to expire, so the loop sends
    /// [`Msg::Tick`]: a spinner (a turn, a classifier call, a running
    /// helper) or a key's pending second press (Ctrl-C, Ctrl-D, a Ctrl-X
    /// chord). An idle terminal is not woken.
    #[must_use]
    pub fn wants_tick(&self) -> bool {
        self.status.busy()
            || self.mode.ticks()
            || self.quit.confirm.is_some()
            || self.transcript.helpers_running()
            || self.transcript.helpers_lingering()
    }

    /// Ask for a bell when enabled and the terminal is unfocused.
    fn notify(&mut self) {
        if self.bell && !self.focused {
            self.ring = true;
        }
    }

    /// A live `[ui]` setting the daemon pushed: the view follows it at once
    /// (`None`: the default).
    pub fn apply_setting(&mut self, key: &str, value: Option<&str>) {
        fn read<T: serde::de::DeserializeOwned + Default>(value: Option<&str>) -> T {
            value
                .and_then(|value| toml::Value::String(value.to_string()).try_into().ok())
                .unwrap_or_default()
        }
        match key {
            "ui.bell" => self.bell = value != Some("false"),
            "ui.checklist" => self.checklist.view = read(value),
            "ui.helpers" => self.transcript.set_helper_view(read(value)),
            _ => {}
        }
    }

    /// Whether the task checklist starts shown (`[ui] checklist`).
    #[must_use]
    pub fn with_checklist_view(mut self, view: ChecklistView) -> Self {
        self.checklist.view = view;
        self
    }

    /// The current session's task checklist.
    pub fn checklist(&self) -> &Checklist {
        &self.checklist.tasks
    }

    /// The checklist the panel under the input shows: `Some` while the
    /// panel is on and the session has tasks.
    pub fn checklist_panel(&self) -> Option<&Checklist> {
        (self.checklist.view.is_shown() && !self.checklist.tasks.is_empty())
            .then_some(&self.checklist.tasks)
    }

    /// Ctrl+T: show or hide the checklist and record the choice. With no
    /// tasks nothing shows either way; a store that could not be read is
    /// named once.
    fn toggle_checklist(&mut self, fx: &mut Vec<Effect>) {
        self.checklist.view = self.checklist.view.toggled();
        fx.push(Effect::SetChecklistView(self.checklist.view));
        if let Some(why) = self.checklist.note.take() {
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

    /// Record the transcript viewport height (set by the renderer).
    pub fn set_viewport_height(&mut self, height: usize) {
        self.viewport_height = height.max(1);
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

    /// Whether `msg` is word from the agent or its run (an event of the
    /// session or of a helper, the classifier, a compaction, an approval
    /// request, a run the daemon reports): the liveness the prompt marker
    /// shows ([`StatusInfo::busy`]). Keys, ticks and the client's own
    /// answers are not.
    fn from_the_agent(msg: &Msg) -> bool {
        matches!(
            msg,
            Msg::Agent(_)
                | Msg::Scoped(_)
                | Msg::RemoteTurn { .. }
                | Msg::Peer { .. }
                | Msg::Triggered { .. }
                | Msg::Diff { .. }
                | Msg::ToolNote { .. }
                | Msg::Approval(_)
                | Msg::Escalation(..)
                | Msg::EscalationResolved(_)
                | Msg::SteerDelivered(_)
                | Msg::CompactionRun(_)
                | Msg::Status(StatusUpdate::Classifying(_) | StatusUpdate::ClassifierCheck(_))
        )
    }

    /// Process one message. Pure: no I/O, only state changes and returned effects.
    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        self.update_at(msg, Instant::now())
    }

    /// [`Self::update`] with an explicit clock (for tests).
    pub fn update_at(&mut self, msg: Msg, now: Instant) -> Vec<Effect> {
        let mut fx = Vec::new();
        let mut dirty = true;
        if Self::from_the_agent(&msg) {
            self.status.heard_at = Some(now);
        }
        match msg {
            Msg::Key(key) => {
                if self.mouse.selection.is_some() {
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
                    // A dialog the user opens with this key is armed.
                    self.arming = None;
                    self.main_key(&key, now, &mut fx);
                }
            }
            Msg::Paste(text) => self.paste_text(&text),
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
                        self.turn.streamed();
                    }
                    AgentEvent::TextComplete { content, .. } if !content.is_empty() => {
                        self.turn.streamed();
                    }
                    // The gauge's reading is the daemon's (`rkat/cost`); a
                    // compaction re-arms its warning.
                    // Its folded line's summary is read from the record.
                    AgentEvent::CompactionCompleted { .. } => {
                        self.gauge.compacted();
                        fx.push(Effect::ReadCompactionSummary);
                    }
                    _ => {}
                }
                self.status.track_tools(&env.payload, now);
                match &env.payload {
                    AgentEvent::TurnStarted { .. } => self.status.model_since = Some(now),
                    AgentEvent::ToolCallRequested { .. }
                    | AgentEvent::ToolExecutionStarted { .. }
                    | AgentEvent::TurnCompleted { .. }
                    | AgentEvent::RunCompleted { .. }
                    | AgentEvent::RunFailed { .. } => self.status.model_since = None,
                    _ => {}
                }
                match &env.payload {
                    AgentEvent::CompactionStarted { .. } => {
                        self.status.compacting_since = Some(now);
                    }
                    AgentEvent::CompactionCompleted { .. }
                    | AgentEvent::CompactionFailed { .. }
                    | AgentEvent::RequestedCompactionSkipped { .. }
                    | AgentEvent::RunCompleted { .. }
                    | AgentEvent::RunFailed { .. } => self.status.compacting_since = None,
                    _ => {}
                }
                // How a failure is said ([`failures`]): an empty answer's
                // retries draw nothing and its failure is one line; any
                // failure the run said is not said again at the turn's end.
                let shown = match &env.payload {
                    AgentEvent::Retrying { retry, .. }
                        if failures::is_empty_answer_retry(retry) =>
                    {
                        self.failures.empty_retries += 1;
                        false
                    }
                    AgentEvent::RunFailed { error_report, .. }
                        if failures::is_empty_answer(error_report) =>
                    {
                        let (level, text) = self.failures.empty_answer_line();
                        self.failures.said = true;
                        self.transcript.push_notice(level, text);
                        false
                    }
                    AgentEvent::RunFailed { error_report, .. } => {
                        self.failures.said |= !matches!(
                            error_report.class,
                            meerkat_core::AgentErrorClass::MaxTurns
                                | meerkat_core::AgentErrorClass::Cancelled
                        );
                        true
                    }
                    _ => true,
                };
                if shown
                    && let Some(usage) = self.transcript.apply_agent_event_at(&env.payload, now)
                {
                    self.status
                        .add_tokens(usage.input_tokens, usage.output_tokens);
                }
            }
            Msg::Scoped(ev) => {
                self.transcript.apply_scoped_event_at(&ev, now);
            }
            Msg::TurnFinished(outcome) => {
                self.turn_finished(outcome, now, &mut fx);
                // Not when the queued messages started the next turn.
                if !self.turn.is_running() {
                    self.notify();
                }
            }
            Msg::SteerAck { id, result } => self.steer_ack(id, result, now, &mut fx),
            Msg::SteerDelivered(id) => self.pending.show_delivered(&mut self.transcript, id),
            Msg::Recalled { recalled, errors } => self.recalled(&recalled, &errors),
            Msg::Returned(ids) => self.returned(&ids),
            Msg::BtwAnswered(answer) => self.btw_answered(answer),
            Msg::NotDelivered { id, why } => self.not_delivered(id, why.as_deref()),
            Msg::Unkept { text, why } => self.unkept(text, &why),
            Msg::QuitDecided(decision) => self.quit_decided(decision, &mut fx),
            Msg::Rerooted { workspace, realm } => {
                let home = self.transcript.home().map(std::path::Path::to_path_buf);
                self.transcript
                    .set_paths(Some(workspace.as_path().to_path_buf()), home);
                self.workspace = Some(workspace);
                self.completion.path_index = PathIndex::Dropped;
                self.completion.generation += 1;
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
                self.escalation(prompt, id, now);
                self.notify();
            }
            Msg::Focus(focused) => {
                self.focused = focused;
                dirty = false;
            }
            Msg::EscalationResolved(id) => self.escalation_resolved(id, now),
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
            Msg::Notice { level, text } => {
                self.transcript.push_notice(level, text);
            }
            Msg::Setting { key, value } => self.apply_setting(&key, value.as_deref()),
            Msg::Status(update) => {
                // Envelope sequence numbers restart with every session task.
                if matches!(update, StatusUpdate::Session(_)) {
                    self.last_seq = 0;
                }
                if let StatusUpdate::Agent(agent) = &update {
                    self.transcript.set_agent_name(agent.clone());
                }
                if let StatusUpdate::Classifying(running) = &update {
                    self.status.classifying_since = if *running {
                        self.status.classifying_since.or(Some(now))
                    } else {
                        None
                    };
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
                self.show_modal(Modal::Resume(ResumePicker::new(items)));
            }
            Msg::ShowAgentPicker(agents) => self.show_picker(picker::agent_picker(&agents), false),
            Msg::RefreshAgentPicker(agents) => {
                self.show_picker(picker::agent_picker(&agents), true);
            }
            Msg::AgentPickerRefused(why) => {
                self.picker_refused(PickerKind::Agents, &format!("not retired: {why}"));
            }
            Msg::PickerRefused { kind, why } => self.picker_refused(kind, &why),
            Msg::PickerDone { kind, text } => self.picker_done(kind, &text),
            Msg::ShowPicker { picker, refresh } => self.show_picker(picker, refresh),
            Msg::RehomeChecked { typed, answer } => self.rehome_checked(&typed, answer),
            Msg::ShowClassifierPicker(groups) => {
                if matches!(self.modal, None | Some(Modal::Help(_))) && self.parked.is_none() {
                    self.modal = Some(Modal::Model(ModelPicker::new(groups).for_classifier()));
                }
            }
            Msg::ShowModelPicker(groups) => {
                if matches!(self.modal, None | Some(Modal::Help(_))) && self.parked.is_none() {
                    self.modal = Some(Modal::Model(ModelPicker::new(groups)));
                } else {
                    for warning in ModelPicker::warnings(&groups) {
                        self.transcript.push_notice(NoticeLevel::Warn, warning);
                    }
                }
            }
            Msg::RemoteTurn { prompt } => self.remote_turn(prompt, now),
            Msg::Peer { from, text } => {
                // A message that wakes the idle agent comes just before
                // its run starts: that run is not the user's.
                // (Or just after it, when the client joined the run on
                // the daemon's word first.)
                if !text.is_empty()
                    && let Some(origin) = self.turn.wake(format!(" to {from}'s message"))
                    && self.failures.remote
                    && self.failures.origin.is_none()
                {
                    self.failures.origin = Some(origin);
                }
                self.transcript
                    .peer_message(self.run_first_block, from, text);
            }
            Msg::Triggered {
                label,
                prompt,
                summary,
            } => {
                self.failures.origin = Some(format!(" ({label})"));
                self.transcript
                    .label_triggered(self.run_first_block, label, prompt, summary);
            }
            Msg::SeatedHelpers(members) => self.transcript.seated_helpers(members),
            Msg::HelperEnded { path, how, told } => {
                self.transcript.helper_ended(&path, &how, told.as_deref());
            }
            Msg::ResetTranscript(blocks) => {
                self.clear_selection();
                self.panel_selected = None;
                self.helper_view = None;
                self.transcript.reset(blocks);
                self.last_seq = 0;
                // Keep placeholders for messages still in flight.
                self.pending.restore_placeholders(&mut self.transcript);
                if let Some(run) = &mut self.shell.running {
                    run.block = self.transcript.push(Block::Shell {
                        command: run.command.clone(),
                        state: ShellState::Running { since: run.since },
                        output: None,
                        collapsed: false,
                    });
                }
                self.run_first_block = self.transcript.next_id();
                self.scroll.bottom();
            }
            Msg::CompactRequested(result) => self.compact_requested(result, now, &mut fx),
            Msg::CompactionRun(running) => self.compaction_run(running, now, &mut fx),
            Msg::CompactionSummary(read) => dirty = self.transcript.fill_compaction(read),
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
                self.checklist.tasks = list;
                self.checklist.note = None;
            }
            Msg::ChecklistUnavailable(why) => self.checklist.note = Some(why),
            Msg::Mentions { attached, sealed } => self.note_mentions(&attached, &sealed),
            Msg::PathIndex { generation, paths } => {
                if self.completion.path_index == PathIndex::Indexing(generation) {
                    self.completion.path_index = PathIndex::Ready(paths);
                    // Rebuild the menu for the word under the cursor.
                    self.completion.menu = None;
                }
            }
            // Sent only while [`Self::wants_tick`].
            Msg::Tick => {
                let helpers_spun = self.transcript.tick_helpers(now);
                let tools_ticked = self.transcript.tick_tools(now);
                // An ended helper leaving the panel redraws it.
                let lingering = self.transcript.helpers_lingering();
                dirty = helpers_spun
                    || tools_ticked
                    || lingering
                    || self.status.busy()
                    || self.mode.ticks()
                    || self.quit.confirm.is_some();
                self.mode.expire(now);
                if self.quit.confirm.is_some() && !self.quit_confirm_armed(now) {
                    self.quit.confirm = None;
                }
            }
        }
        self.status.queued = self.pending.len();
        self.status.send_now = self.turn.is_running() && self.has_waiting_messages();
        let asking = matches!(self.modal, Some(Modal::Escalation { .. }))
            || !self.pending_escalations.is_empty();
        self.status.asking_since = if asking {
            self.status.asking_since.or(Some(now))
        } else {
            None
        };
        self.transcript.settle_helpers(now);
        self.settle_panel();
        self.status.helper_panel = !self.panel_rows.is_empty();
        self.follow_helper_picker();
        self.refresh_menu();
        self.dirty |= dirty;
        fx
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
