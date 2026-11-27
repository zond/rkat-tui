//! Transcript model: the list of rendered blocks and the agent-event → block mapping.
//!
//! The mapping mirrors `meerkat-cli/src/stream_renderer.rs` but produces structured
//! blocks instead of ANSI text. Each block has a per-width layout cache that is
//! invalidated whenever the block mutates.
//!
//! Prose: the text of one model step is one [`Block::Assistant`] unless
//! something visible sits between two of its deltas: a tool call, a
//! notice, or a reasoning block with non-blank text. Reasoning that is
//! empty or only whitespace never closes a prose block and never becomes a
//! block of its own; an empty text delta changes nothing; the step's
//! `TextComplete` closes nothing (the next step's `TurnStarted` does).
//! Text that continues with nothing visible between is appended as it
//! arrives, with no separator: the engine's events name one assistant
//! message per step and do not say where a provider's text parts or
//! message items end, and the engine itself joins them as they are (its
//! `TextComplete` and the run's text), so a cut inside a word or a quote
//! stays invisible. A stored conversation follows the same rule
//! ([`blocks_from_messages`]), and so does a helper's view (the same
//! code path, scoped to its header).
//!
//! Scopes: the session and each helper (under its header) stream on
//! their own, and only something visible of a prose block's own scope
//! closes it (its tool call, its non-blank reasoning, a notice of its, its
//! step's end). A block of another scope that arrives while it is open
//! (a helper's tool line, header, task, error or ending while the agent's
//! answer streams; the agent's tool call while a helper's does) is
//! appended after it and the prose goes on in its block: the open prose
//! keeps its place, where it began, and grows there, so a link or a
//! table cut by a helper's line in the event order is still one block and
//! draws whole, with the other scope's lines below it in their own order.
//! Ids, owners and folds are those of any appended block (nothing is
//! moved or held back). A stored conversation has no such interleaving:
//! it holds the agent's messages only, and its helpers' blocks are not
//! restored between them.

use std::time::Instant;

use indexmap::IndexMap;
use meerkat_core::types::text_content;
use meerkat_core::{AgentEvent, HookPoint, TurnUsage};
use ratatui::text::Line;
use serde_json::Value;

use super::delivery::PendingId;
use super::render::SPINNER;
use crate::text::truncate_chars;

mod helpers;
mod layout;
mod notices;
pub mod outcome;
#[cfg(any(test, feature = "test-support"))]
mod probe;
mod restore;
mod runs;
pub mod tool_line;

#[cfg(any(test, feature = "test-support"))]
pub use helpers::PANEL_LINGER;
pub use helpers::{
    HelperFold, HelperKind, HelperLast, HelperState, HelperView, MAX_HELPER_PREVIEW,
};
use helpers::{Helpers, helper_args_preview};
use layout::LayoutCache;
use notices::{compaction_skipped, retry_notice, upstream_notice};
pub use restore::blocks_from_messages;
use runs::{RunRole, role_lines};

/// Maximum tool-result lines kept in the preview (matches the CLI stream renderer).
pub const MAX_TOOL_RESULT_LINES: usize = 20;

/// Most notes kept for tool calls whose block has not arrived yet.
const MAX_PENDING_NOTES: usize = 64;

/// A block of the transcript: given in the order blocks are added and
/// never reused, so it names the same block for its life and orders as the
/// blocks do (a removal shifts nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub usize);

/// Severity of a notice block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    /// Informational chrome.
    Info,
    /// Something degraded but the run continues.
    Warn,
    Error,
}

impl NoticeLevel {
    /// A warning when `warn`, else information: a notice the daemon sent
    /// with its `warn` flag.
    #[must_use]
    pub fn of_warning(warn: bool) -> Self {
        if warn { Self::Warn } else { Self::Info }
    }
}

/// Lifecycle state of a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    /// The model asked for the call; approval/dispatch pending.
    Requested,
    /// The tool is executing.
    Running {
        /// When execution started.
        since: Instant,
    },
    /// Execution finished.
    Done {
        /// Whether the tool reported an error.
        is_error: bool,
        /// Wall-clock execution time.
        duration_ms: u64,
    },
    /// A hook (approval engine) denied the call.
    Denied {
        /// Human-readable reason.
        reason: String,
    },
}

/// Lifecycle of a `!` command's [`Block::Shell`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellState {
    /// The command is running.
    Running {
        /// When it started.
        since: Instant,
    },
    /// It ended.
    Done {
        exit: crate::user_shell::ShellExit,
        /// Wall-clock time.
        duration_ms: u64,
    },
    /// The user cancelled it (Esc, Ctrl-C).
    Cancelled,
    /// It could not run.
    Failed(String),
}

/// Delivery state shown on a [`Block::Queued`] placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuedLabel {
    /// Waiting for the runtime to admit it.
    Queued,
    /// Admitted; written into the turn at its next model boundary (or run
    /// as a follow-up turn).
    Delivering,
    /// Held by the UI; sent when the current work ends.
    Deferred,
    /// A `!` command held until the turn ends.
    Shell,
    /// A slash command held until the turn ends (`/model`, `/new`, …).
    Command,
}

impl QueuedLabel {
    /// Text shown after the message.
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            // Every steer waiting when a model request is composed joins
            // it: "next" is the first request that starts after it.
            Self::Delivering => "goes in at the agent's next step",
            Self::Deferred => "will send after this turn",
            Self::Shell => "runs after this turn",
            Self::Command => "when the turn ends",
        }
    }
}

impl HelperState {
    /// The word the header shows.
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether the helper is still working.
    #[must_use]
    pub fn is_running(self) -> bool {
        self == Self::Running
    }
}

impl HelperFold {
    /// `[label · model]`, or `[label]` while the model is unknown.
    #[must_use]
    pub fn tag(&self) -> String {
        match &self.model {
            Some(model) => format!("[{} · {model}]", self.label),
            None => format!("[{}]", self.label),
        }
    }

    /// The header's status text after the tag: `running · 4 tool calls ·
    /// grep "fn resolve"`, `done · 7 tool calls · 2.1k tokens`.
    #[must_use]
    pub fn status_text(&self) -> String {
        let parts = self.status_parts();
        match parts.glyph {
            Some(glyph) => format!("{glyph} {}{}", parts.state, parts.rest),
            None => format!("{}{}", parts.state, parts.rest),
        }
    }

    /// [`Self::status_text`] in its parts: the spinner's glyph of a
    /// running helper, its state in words, and what follows.
    #[must_use]
    pub fn status_parts(&self) -> StatusParts {
        let glyph = self.state.is_running().then(|| {
            SPINNER
                .get(self.spin % SPINNER.len())
                .copied()
                .unwrap_or('⠋')
        });
        let state = match self.ended.as_deref().filter(|_| !self.state.is_running()) {
            Some(how) => how.to_string(),
            // A kept helper whose run ended is seated still, idle, until it
            // is retired (the daemon says so).
            None if self.state == HelperState::Done && self.kind.is_some_and(HelperKind::kept) => {
                "idle".to_string()
            }
            None => self.state.text().to_string(),
        };
        StatusParts {
            glyph,
            state,
            rest: self.status_rest(),
        }
    }

    /// What follows the state in [`Self::status_text`]: its calls, then a
    /// running helper's latest activity or an ended one's tokens.
    fn status_rest(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = write!(out, " · {}", crate::text::count(self.calls, "tool call"));
        if self.state.is_running() {
            match &self.last {
                HelperLast::Nothing => {}
                HelperLast::Tool { name, args } if args.is_empty() => {
                    let _ = write!(out, " · {name}");
                }
                HelperLast::Tool { name, args } => {
                    let _ = write!(out, " · {name} {args}");
                }
                HelperLast::Text(text) => {
                    let _ = write!(out, " · {text}");
                }
            }
        } else if let Some(tokens) = self.usage {
            let _ = write!(out, " · {} tokens", super::render::fmt_tokens(tokens));
        }
        out
    }
}

/// A helper's status line in its parts ([`HelperFold::status_parts`]), for
/// each place that shows it to join as it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusParts {
    /// The spinner's glyph, while it runs.
    pub glyph: Option<char>,
    /// Its state in words (`running`, `idle`, `stopped by you`).
    pub state: String,
    /// What follows the state, with its leading ` · `.
    pub rest: String,
}

/// A file diff attached to a tool block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffView {
    /// File the diff applies to.
    pub path: String,
    /// Unified diff text.
    pub unified: String,
    pub added: usize,
    pub removed: usize,
}

/// The helper a peer message was for ([`Block::Peer`]'s `to`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerTo {
    /// The helper's tag, `[scout · model]`.
    pub tag: String,
    /// How deep it is nested (its lines' indent).
    pub depth: usize,
}

/// One rendered unit of the transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// A prompt typed by the user.
    User {
        /// Prompt text.
        text: String,
    },
    /// A message typed while a turn was running, not yet seen by the model.
    /// Replaced in place by [`Block::User`] once delivered.
    Queued {
        /// Pending-queue id.
        id: PendingId,
        text: String,
        /// Delivery state.
        label: QueuedLabel,
    },
    /// Assistant output (markdown source).
    Assistant {
        /// Markdown source accumulated from text deltas.
        md: String,
    },
    /// Model reasoning / thinking.
    Reasoning {
        text: String,
        /// Whether only a one-line summary is shown.
        collapsed: bool,
    },
    /// A tool call and its lifecycle.
    Tool {
        /// The tool's own name (`glob`).
        tool: String,
        /// The helper's tag as its line shows it in front of the call
        /// (`[scout · model]`); `None` for the agent's own call.
        tag: Option<String>,
        /// Arguments as requested by the model.
        args: Value,
        /// Its line's verb and object ([`tool_line::head`]).
        head: tool_line::Head,
        /// What the finished call came to ([`tool_line::outcome`]).
        outcome: tool_line::Outcome,
        /// Whole seconds it has run, while it runs (the line's clock,
        /// advanced by [`Transcript::tick_tools`]).
        ticked: u64,
        state: ToolState,
        /// Truncated result preview.
        result_preview: Option<Preview>,
        /// Attached file diff, if the tool edited a file.
        diff: Option<DiffView>,
        /// One dim line under the header, shown even when collapsed (e.g.
        /// `allowed by classifier: <reason>`).
        note: Option<String>,
        /// Nesting depth (0 for the primary agent, >0 for sub-agents).
        depth: usize,
        /// Whether the arguments, the result preview and the diff are hidden
        /// behind the line. A call starts folded unless the transcript's
        /// detail is on; a click toggles it, Ctrl-O all of them.
        collapsed: bool,
    },
    /// An approval decision.
    Approval {
        /// Tool the decision applies to.
        tool: String,
        /// Tier / mechanism that decided.
        tier: String,
        allowed: bool,
        /// Optional explanation.
        reason: Option<String>,
    },
    /// Chrome line (retry, budget, compaction, errors, command output).
    Notice {
        /// Severity.
        level: NoticeLevel,
        text: String,
    },
    /// A helper's header (see [`HelperFold`]).
    Helper(HelperFold),
    /// A helper's input as its full view shows it, as a prompt (`› `)
    /// under who gave it: its task (`task from main-ab12`) or a later
    /// input ([`HelperFold`]).
    Task {
        /// Who gave it, or what it is.
        label: String,
        text: String,
    },
    /// A message another agent of the workspace sent this one (a peer
    /// message), labelled with its sender.
    Peer {
        /// The sending agent's name.
        from: String,
        /// The message (empty: delivered into a running turn, whose text
        /// the live events do not carry until the run that reads it).
        text: String,
        /// Whether only its first line is shown.
        collapsed: bool,
        /// The helper that received it, when this agent did not: drawn as
        /// that helper's other lines are (its tag, its indent), under its
        /// header, so it never reads as this agent's own message.
        to: Option<PeerTo>,
    },
    /// A detached job's outcome (a `fork_off` child, a council), compact
    /// ([`outcome::JobOutcome`]): a header with its counts and the result's
    /// Markdown, folded to a few lines while `collapsed`.
    Outcome {
        /// `fork kid finished (completed) · 11 turns · …`.
        header: String,
        /// The result's text, and the error of one that failed.
        md: String,
        /// Whether it ended without its result.
        failed: bool,
        /// Whether only the first lines of `md` show.
        collapsed: bool,
        /// The engine's job id (the same outcome is shown once).
        job: Option<String>,
    },
    /// A `/btw` side question and its answer: a note beside the agent's
    /// turn, not part of the conversation (the agent never sees it, and a
    /// conversation shown again from the daemon does not have it).
    Btw {
        question: String,
        /// The answer (`Ok`), why there is none (`Err`), or `None` while
        /// it is pending.
        answer: Option<Result<String, String>>,
    },
    /// A run nobody typed: a schedule fired it, or a file watch
    /// (`scheduled: <name>`, `files changed: …`), with its input.
    Triggered {
        /// What fired it.
        cause: crate::daemon::protocol::TriggerCause,
        /// What fired it, in words.
        label: String,
        /// The run's input.
        text: String,
    },
    /// A run the daemon started with a note of a helper's ending (or its
    /// going idle): one folded line by default (what happened and the
    /// answer's first line), the whole note when opened.
    HelperNote {
        /// What happened (`helper review-x finished`, `2 helpers ended`).
        title: String,
        /// The answer's first line.
        first: String,
        /// The whole note.
        text: String,
        /// The helpers it is about.
        helpers: Vec<String>,
        /// Whether one of them failed.
        failed: bool,
        /// What it says of them.
        kind: crate::daemon::agents::endings::NoteKind,
        /// Whether only the one line shows.
        collapsed: bool,
        /// Whether the daemon's notice of the same ending was left out for
        /// it (the two say the same thing): a later notice of one of its
        /// helpers is another ending.
        paired: bool,
    },
    /// A compaction of the conversation's context: one folded line with
    /// its figures (`compacted: 379 → 68 messages (3.2k summary tokens)`),
    /// the summary the engine wrote in its place when opened. Nothing of
    /// the summary is drawn anywhere else.
    Compaction {
        /// The figures, without the icon.
        line: String,
        /// The summary's text (`None`: not read from the conversation's
        /// record yet; `Err`: why it could not be).
        summary: Option<Result<String, String>>,
        /// Whether only the one line shows.
        collapsed: bool,
    },
    /// A command the user ran with `!`, and its output.
    Shell {
        /// The command line.
        command: String,
        state: ShellState,
        /// The output preview (capped like a tool result).
        output: Option<Preview>,
        /// Whether the output is hidden behind the header.
        collapsed: bool,
    },
}

/// A tool result or a `!` command's output as its block keeps it: the
/// capped preview, and how many lines the whole output had (counted when
/// the preview was made, never read back out of its text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    text: String,
    lines: usize,
}

impl Preview {
    /// The preview of `output`: its first [`MAX_TOOL_RESULT_LINES`] lines,
    /// then `... (N more lines)` when there are more.
    #[must_use]
    pub fn of(output: &str) -> Self {
        let lines = output.lines().count();
        let mut shown: Vec<&str> = output.lines().take(MAX_TOOL_RESULT_LINES).collect();
        let more;
        if lines > MAX_TOOL_RESULT_LINES {
            more = format!("... ({} more lines)", lines - MAX_TOOL_RESULT_LINES);
            shown.push(&more);
        }
        Self {
            text: shown.join("\n"),
            lines,
        }
    }

    /// How many lines the whole output had.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines
    }
}

impl std::ops::Deref for Preview {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl PartialEq<str> for Preview {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

impl PartialEq<&str> for Preview {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl std::fmt::Display for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// A preview that is the whole output (it counts its own lines).
impl From<&str> for Preview {
    fn from(text: &str) -> Self {
        Self::from(text.to_string())
    }
}

/// A preview that is the whole output (it counts its own lines).
impl From<String> for Preview {
    fn from(text: String) -> Self {
        let lines = text.lines().count();
        Self { text, lines }
    }
}

/// Cached wrapped lines for one block at one width.
#[derive(Debug, Clone)]
pub struct CachedLayout {
    /// Width the lines were computed for.
    pub width: u16,
    /// Wrapped lines (including the trailing spacer line).
    pub lines: Vec<Line<'static>>,
}

/// The transcript: blocks, tool-id index and per-block layout cache.
#[derive(Debug, Default)]
pub struct Transcript {
    /// The blocks in order, each with its id, layouts and owner.
    entries: Vec<Entry>,
    /// The id the next block gets.
    next_id: usize,
    by_tool_id: IndexMap<String, BlockId>,
    /// The open prose and reasoning blocks deltas append to: the session's
    /// own under `None`, each helper's under its header.
    streams: IndexMap<Option<BlockId>, Stream>,
    /// A deciding prose block's words not shown yet, by its scope
    /// ([`Stream::deciding`]): a start of the line `thought`, held so a
    /// model's reasoning sent as text never flashes as a reply.
    held: IndexMap<Option<BlockId>, String>,
    helpers: Helpers,
    /// Where tool lines show paths from.
    paths: tool_line::PathView,
    /// Whether every foldable block is shown in full (Ctrl-O): new ones
    /// arrive so.
    detail: bool,
    /// Notes for tool calls whose block has not arrived yet, by tool-use id
    /// (at most [`MAX_PENDING_NOTES`], oldest dropped first).
    pending_notes: IndexMap<String, String>,
    /// The name of each peer this conversation's agent or its helpers can
    /// message, by peer id. Learnt from the `contacts` results this
    /// transcript showed (and a stored `peers` result of an earlier build).
    /// A `send_message` block, the agent's or a helper's, names its
    /// recipient with them.
    peer_names: IndexMap<String, String>,
    /// Runs nobody here typed that one of their two reports showed (the
    /// daemon's note of the run, [`Self::label_triggered`], or the run's
    /// own input, [`Self::remote_input`]), by their input and which
    /// report it was: the other report of the same run shows nothing
    /// again (at most [`MAX_ANNOUNCED`], the oldest dropped).
    announced: Vec<(String, Heard)>,
    /// The laid-out lines before each block ([`Self::ensure_layout`]).
    layout: LayoutCache,
    /// The blocks of the runs opened by a click ([`runs`]): a run is
    /// opened while one of its blocks is here.
    open_runs: std::collections::HashSet<BlockId>,
}

/// One block of the transcript and what is kept about it.
#[derive(Debug)]
struct Entry {
    id: BlockId,
    block: Block,
    /// Its layout at the transcript's width (folded helpers hide it).
    layout: Option<CachedLayout>,
    /// Its layout at one width as a helper's full view draws it (folded
    /// helpers do not hide it there).
    view_layout: Option<CachedLayout>,
    /// Its part in a run as its `layout` draws it ([`runs`]).
    role: RunRole,
    /// Its part in a run as its `view_layout` draws it.
    view_role: RunRole,
    /// The header of the helper it belongs to (`None`: the session's own
    /// blocks and top-level helper headers).
    owner: Option<BlockId>,
}

impl Entry {
    /// Its block changed: both its layouts are stale.
    fn forget_layouts(&mut self) {
        self.layout = None;
        self.view_layout = None;
    }
}

/// One agent's streaming state (the session's or a helper's): the prose
/// and reasoning blocks its deltas append to, and whether its current
/// model call streamed any (so the completion does not repeat it).
#[derive(Debug, Clone, Copy, Default)]
struct Stream {
    assistant: Option<BlockId>,
    reasoning: Option<BlockId>,
    text_seen: bool,
    reasoning_seen: bool,
    /// The open prose block's first line is not decided yet: it may be a
    /// model's reasoning sent as text ([`leaked_thought`]); its words are
    /// held in [`Transcript::held`] until it is.
    deciding: bool,
    /// The open reasoning block is such text: the step's text goes on in
    /// it.
    thought: bool,
}

/// The line a model that sends its reasoning as ordinary text (a
/// Gemini-family model behind an OpenAI-compatible proxy) opens it with.
const THOUGHT_LINE: &str = "thought";

/// What a prose block's text says of itself so far.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Thought<'a> {
    /// It opens with the line `thought` (after any blank space): the
    /// model's reasoning, which is the rest.
    Is(&'a str),
    /// It could still: blank, or a start of that line.
    Undecided,
    /// It does not.
    Not,
}

/// Whether `text` opens with exactly the line `thought` (leading blank
/// space and blank lines allowed, nothing else on the line), which marks
/// a model's reasoning sent as ordinary text; the rest is the reasoning.
/// A block that merely begins with the word (`thought about it`,
/// `Thought: x`) is not.
pub(crate) fn leaked_thought(text: &str) -> Thought<'_> {
    let start = text.trim_start();
    match start.strip_prefix(THOUGHT_LINE) {
        Some(rest) => match rest
            .strip_prefix("\r\n")
            .or_else(|| rest.strip_prefix('\n'))
        {
            Some(reasoning) => Thought::Is(reasoning),
            None if rest.is_empty() || rest == "\r" => Thought::Undecided,
            None => Thought::Not,
        },
        None if THOUGHT_LINE.starts_with(start) => Thought::Undecided,
        None => Thought::Not,
    }
}

/// Who a helper's event belongs to ([`Transcript::apply_scoped_event_at`]).
struct HelperScope<'a> {
    /// The helper's header block.
    header: BlockId,
    /// Its label as block names show it.
    tag: String,
    /// The stream scope id its tool-call ids are qualified with.
    scope_id: &'a str,
    /// How deep it is nested (its tool blocks' indent).
    depth: usize,
    /// Who gave it its task: the helper above it, or the agent.
    parent: Option<String>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// The transcript as plain text for `/export`: one `## ` header per
    /// shown block (user, assistant, reasoning, tool, approval, notice,
    /// helper, queued) and its text. Folded reasoning, collapsed tool output
    /// and the blocks of folded helpers keep only their headers, as on
    /// screen.
    #[must_use]
    pub fn export_text(&self) -> String {
        self.export_from(BlockId(0), true)
    }

    /// The blocks from `from` on as [`Self::export_text`] writes them,
    /// without the `/btw` notes: what the screen shows of the running turn
    /// (a `/btw` reads it; the session's stored history does not have the
    /// run yet).
    #[must_use]
    pub fn turn_text(&self, from: BlockId) -> String {
        self.export_from(from, false)
    }

    fn export_from(&self, from: BlockId, side_notes: bool) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for Entry { id, block, .. } in &self.entries {
            if *id < from
                || self.is_hidden(*id)
                || (!side_notes && matches!(block, Block::Btw { .. }))
            {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            let body = |out: &mut String, text: &str| {
                let text = text.trim_end();
                if !text.is_empty() {
                    out.push_str(text);
                    out.push('\n');
                }
            };
            match block {
                Block::User { text } => {
                    out.push_str("## user\n");
                    body(&mut out, text);
                }
                Block::Queued { text, label, .. } => {
                    let _ = writeln!(out, "## user ({})", label.text());
                    body(&mut out, text);
                }
                Block::Assistant { md } => {
                    out.push_str("## assistant\n");
                    body(&mut out, md);
                }
                Block::Reasoning { text, collapsed } => {
                    out.push_str("## reasoning\n");
                    if !collapsed {
                        body(&mut out, text);
                    }
                }
                Block::Tool {
                    tool,
                    args,
                    head,
                    outcome,
                    ticked,
                    state,
                    result_preview,
                    diff,
                    note,
                    collapsed,
                    ..
                } => {
                    let parts = tool_line::Parts::of(head, state, outcome, *ticked, diff.as_ref());
                    let _ = writeln!(out, "## tool {}", parts.text());
                    if let Some(note) = note {
                        let _ = writeln!(out, "{note}");
                    }
                    if !collapsed {
                        if let ToolState::Denied { reason } = state {
                            body(&mut out, reason);
                        }
                        if let Some(args) = tool_line::expanded_args(tool, args) {
                            body(&mut out, &args);
                        }
                        if let Some(preview) = result_preview {
                            body(&mut out, preview);
                        }
                        if let Some(diff) = diff {
                            body(&mut out, &diff.unified);
                        }
                    }
                }
                Block::Shell {
                    command,
                    state,
                    output,
                    collapsed,
                } => {
                    let _ = writeln!(out, "## shell $ {command} ({})", shell_state_text(state));
                    if let (Some(output), false) = (output, collapsed) {
                        body(&mut out, output);
                    }
                }
                Block::Peer { from, text, to, .. } => {
                    match to {
                        Some(to) => {
                            let _ = writeln!(out, "## message to helper {} from {from}", to.tag);
                        }
                        None => {
                            let _ = writeln!(out, "## message from agent {from}");
                        }
                    }
                    body(&mut out, text);
                }
                Block::Task { label, text } => {
                    let _ = writeln!(out, "## {label}");
                    body(&mut out, text);
                }
                Block::Triggered { label, text, .. } => {
                    let _ = writeln!(out, "## {label}");
                    body(&mut out, text);
                }
                Block::HelperNote { title, text, .. } => {
                    let _ = writeln!(out, "## {title} (the note the agent was given)");
                    body(&mut out, text);
                }
                Block::Compaction { line, summary, .. } => {
                    let _ = writeln!(out, "## {line} (the summary the agent goes on from)");
                    body(&mut out, &compaction_body(summary.as_ref()));
                }
                Block::Btw { question, answer } => {
                    let _ = writeln!(
                        out,
                        "## btw (a side question, not part of the conversation): {question}"
                    );
                    match answer {
                        Some(Ok(text)) => body(&mut out, text),
                        Some(Err(why)) => body(&mut out, &format!("(no answer: {why})")),
                        None => body(&mut out, "(no answer yet)"),
                    }
                }
                Block::Outcome { header, md, .. } => {
                    let _ = writeln!(out, "## {header}");
                    body(&mut out, md);
                }
                Block::Approval {
                    tool,
                    tier,
                    allowed,
                    reason,
                } => {
                    let verdict = if *allowed { "allowed" } else { "denied" };
                    let _ = write!(out, "## approval: {tool} {verdict} by {tier}");
                    if let Some(reason) = reason {
                        let _ = write!(out, ": {reason}");
                    }
                    out.push('\n');
                }
                Block::Notice { level, text } => {
                    let level = match level {
                        NoticeLevel::Info => "info",
                        NoticeLevel::Warn => "warning",
                        NoticeLevel::Error => "error",
                    };
                    let _ = writeln!(out, "## notice ({level})");
                    body(&mut out, text);
                }
                Block::Helper(fold) => match fold.kind {
                    Some(kind) => {
                        let _ = writeln!(
                            out,
                            "## helper {} ({}) {}",
                            fold.tag(),
                            kind.text(),
                            fold.state.text()
                        );
                    }
                    None => {
                        let _ = writeln!(out, "## helper {} {}", fold.tag(), fold.state.text());
                    }
                },
            }
        }
        out
    }

    /// Whether `block` is inside a folded helper (the renderer skips it).
    pub fn is_hidden(&self, block: BlockId) -> bool {
        let mut at = block;
        while let Some(up) = self.owner_of(at) {
            if matches!(self.block(up), Some(Block::Helper(f)) if !f.expanded) {
                return true;
            }
            at = up;
        }
        false
    }

    /// The position of block `id` in the transcript.
    fn index_of(&self, id: BlockId) -> Option<usize> {
        self.entries
            .binary_search_by_key(&id, |entry| entry.id)
            .ok()
    }

    fn entry(&self, id: BlockId) -> Option<&Entry> {
        self.entries.get(self.index_of(id)?)
    }

    /// The header of the helper `id` belongs to.
    pub(super) fn owner_of(&self, id: BlockId) -> Option<BlockId> {
        self.entry(id)?.owner
    }

    /// The blocks in order, with their ids.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (BlockId, &Block)> {
        self.entries.iter().map(|entry| (entry.id, &entry.block))
    }

    /// The id the next block will get: every block added from now on has
    /// this id or a later one.
    pub fn next_id(&self) -> BlockId {
        BlockId(self.next_id)
    }

    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.entry(id).map(|entry| &entry.block)
    }

    /// Block id registered for a tool-use id.
    pub fn tool_block(&self, tool_use_id: &str) -> Option<BlockId> {
        self.by_tool_id.get(tool_use_id).copied()
    }

    /// Append a block and return its id. Closes the session's open
    /// streaming blocks.
    pub fn push(&mut self, block: Block) -> BlockId {
        self.close_streams(None);
        self.push_raw(block)
    }

    /// Append a block of `scope` (the session's when `None`, else the
    /// helper whose header it is, which owns it): it closes that scope's
    /// open streaming blocks only, never another scope's (see the module
    /// doc).
    fn push_scoped(&mut self, scope: Option<BlockId>, block: Block) -> BlockId {
        self.close_streams(scope);
        let id = self.push_raw(block);
        if scope.is_some() {
            self.set_owner(id, scope);
        }
        id
    }

    /// Append a notice block.
    pub fn push_notice(&mut self, level: NoticeLevel, text: impl Into<String>) -> BlockId {
        self.push(Block::Notice {
            level,
            text: text.into(),
        })
    }

    /// Remove every block (the helper view setting stays).
    pub fn clear(&mut self) {
        let helpers = std::mem::take(&mut self.helpers).cleared();
        let paths = std::mem::take(&mut self.paths);
        let detail = self.detail;
        *self = Self::default();
        self.helpers = helpers;
        self.paths = paths;
        self.detail = detail;
    }

    /// Replace the whole transcript with `blocks` (e.g. after resuming a
    /// session). Helper headers from history start folded and finished.
    pub fn reset(&mut self, blocks: Vec<Block>) {
        self.clear();
        for mut block in blocks {
            if let Block::Helper(fold) = &mut block {
                fold.expanded = false;
                fold.was_live = false;
                fold.left = None;
                if fold.state.is_running() {
                    fold.state = HelperState::Done;
                }
            }
            if let Block::Tool { tool, args, .. } = &block {
                // The helpers its calls started are its helpers (their
                // messages fold into runs).
                let (tool, args) = (tool.clone(), args.clone());
                self.note_spawn(&tool, &args);
            }
            self.push_raw(block);
        }
        self.refresh_tool_heads();
    }

    /// The block at `index` changed: its layouts and every line count from
    /// it on are stale. The one way a block's change reaches the caches.
    fn invalidate(&mut self, index: usize) -> Option<&mut Entry> {
        self.layout.touch(index);
        let entry = self.entries.get_mut(index)?;
        entry.forget_layouts();
        Some(entry)
    }

    /// Whether the block at `index` is hidden may have changed (its owner,
    /// or a fold above it): its transcript layout and the line counts from
    /// it on are stale. Its full-view layout stays: the full view draws it
    /// whatever folds hide it in the transcript.
    fn invalidate_shown(&mut self, index: usize) -> Option<&mut Entry> {
        self.layout.touch(index);
        let entry = self.entries.get_mut(index)?;
        entry.layout = None;
        Some(entry)
    }

    /// Mutable access to a block; invalidates its layout.
    pub fn block_mut(&mut self, id: BlockId) -> Option<&mut Block> {
        let index = self.index_of(id)?;
        Some(&mut self.invalidate(index)?.block)
    }

    /// Note that a run's input `text` was shown by its `heard` report.
    fn announce(&mut self, text: String, heard: Heard) {
        if self.announced.len() >= MAX_ANNOUNCED {
            let _oldest = self.announced.remove(0);
        }
        self.announced.push((text, heard));
    }

    /// Whether the run with input `text` was shown by its `heard` report
    /// already; that record is taken.
    fn take_announced(&mut self, text: &str, heard: Heard) -> bool {
        let Some(at) = self
            .announced
            .iter()
            .position(|(shown, by)| *by == heard && shown == text)
        else {
            return false;
        };
        let _taken = self.announced.remove(at);
        true
    }

    /// `text`, the input of a run this client did not start (another
    /// client's message, a follow-up run of the turn, a run the daemon
    /// started), as the transcript shows it:
    ///
    /// - nothing when the daemon's note of the run showed it already
    ///   ([`Self::label_triggered`], in either order of the two);
    /// - a note of helpers' endings as its one folded line, never as the
    ///   user's message, whichever path it came by;
    /// - notes held after an interrupt that travel with another client's
    ///   message as their folded line, then the message's own words;
    /// - anything else as the user's message.
    pub fn remote_input(&mut self, text: String) {
        if self.take_announced(&text, Heard::Triggered) {
            return;
        }
        // The daemon's own words: its notice or its triggered line says
        // so already, and they are never the user's band.
        if composed_input(&text).is_some() {
            return;
        }
        if let Some(note) = self.folded_note(&text, true) {
            self.push(note);
            self.announce(text, Heard::Input);
            return;
        }
        if let Some((held, words)) = split_held(&text) {
            if !held.is_empty()
                && let Some(note) = self.folded_note(held, false)
            {
                self.push(note);
            }
            if !words.is_empty() {
                self.push(Block::User {
                    text: words.to_string(),
                });
            }
            return;
        }
        self.push(Block::User { text });
    }

    /// The folded [`Block::HelperNote`] of `text` when it is a note of
    /// helpers' endings in the daemon's words (`whole`: only then; else
    /// any text, folded under its first line), paired with the daemon's
    /// notice of the same ending, which goes.
    fn folded_note(&mut self, text: &str, whole: bool) -> Option<Block> {
        use crate::daemon::agents::{HELPER_ENDED_LABEL, endings};
        if whole {
            endings::note_summary(text)?;
        }
        let mut note = helper_note(HELPER_ENDED_LABEL, text, None, !self.detail)?;
        if let Block::HelperNote {
            helpers, paired, ..
        } = &mut note
        {
            *paired = self.drop_ending_notices(helpers);
        }
        Some(note)
    }

    /// A run nobody typed was fired by `cause` (named `label`) with input
    /// `prompt`: the prompt's block among the run's (`run_first` and
    /// later) is relabelled, else the labelled block is appended. When the
    /// run's own input showed it already ([`Self::remote_input`]) nothing
    /// is. `summary`: a note of helpers' endings' folded line, as the
    /// daemon made it.
    pub fn label_triggered(
        &mut self,
        run_first: BlockId,
        (cause, label): (crate::daemon::protocol::TriggerCause, String),
        prompt: String,
        summary: Option<crate::daemon::agents::endings::NoteSummary>,
    ) {
        if self.take_announced(&prompt, Heard::Input) {
            return;
        }
        let input = prompt.clone();
        let at = self
            .iter()
            .rev()
            .take_while(|(id, _)| *id >= run_first)
            .find(|(_, block)| matches!(block, Block::User { text } if *text == prompt))
            .map(|(id, _)| id);
        let block = match helper_note(&label, &prompt, summary, !self.detail) {
            Some(mut note) => {
                // The notice of the same ending says what the line says:
                // it goes (or, coming later, is not drawn).
                if let Block::HelperNote {
                    helpers, paired, ..
                } = &mut note
                {
                    *paired = self.drop_ending_notices(helpers);
                }
                note
            }
            None => Block::Triggered {
                cause,
                label,
                text: prompt,
            },
        };
        match at {
            Some(id) => {
                self.replace_block(id, block);
            }
            None => {
                self.push(block);
                // The run's input, when it comes, is shown here.
                self.announce(input, Heard::Triggered);
            }
        }
    }

    /// Remove the daemon's notices of how `helpers` ended and who was told
    /// (`helper [review-x · m] finished; main was told`) since the user's
    /// last message: a [`Block::HelperNote`] says it. Whether one went.
    fn drop_ending_notices(&mut self, helpers: &[String]) -> bool {
        use crate::daemon::agents::endings::TOLD;
        let heads: Vec<String> = helpers
            .iter()
            .map(|label| self.ending_head(label))
            .collect();
        // The newest such notice of each helper: an earlier one was of
        // another ending (a kept helper goes idle more than once).
        let gone: Vec<BlockId> = heads
            .iter()
            .filter_map(|head| {
                self.iter()
                    .rev()
                    .take_while(|(_, block)| !matches!(block, Block::User { .. }))
                    .find(|(_, block)| {
                        matches!(block, Block::Notice { text, .. }
                            if text.ends_with(TOLD) && text.starts_with(head.as_str()))
                    })
                    .map(|(id, _)| id)
            })
            .collect();
        for id in &gone {
            self.remove_block(*id);
        }
        !gone.is_empty()
    }

    /// Whether the daemon's notice that `label` ended, told as `told`, is
    /// said already by a folded note of the run it started (since the
    /// user's last message, not yet paired with a notice); that note is
    /// paired with it then.
    pub(super) fn ending_shown(&mut self, label: &str, told: &str) -> bool {
        if !told.ends_with(crate::daemon::agents::endings::TOLD) {
            return false;
        }
        let note = self
            .iter()
            .rev()
            .take_while(|(_, block)| !matches!(block, Block::User { .. }))
            .find(|(_, block)| {
                matches!(block, Block::HelperNote { helpers, paired: false, .. }
                    if helpers.iter().any(|h| h == label))
            })
            .map(|(id, _)| id);
        let Some(id) = note else {
            return false;
        };
        if let Some(Block::HelperNote { paired, .. }) = self.block_mut(id) {
            *paired = true;
        }
        true
    }

    /// A message from the agent `from`: with its `text`, it fills in the
    /// first block of that sender since `run_first` that has none yet (the
    /// message reached a running turn, whose events named only the sender;
    /// the run that reads it carries the text), else it is a new block,
    /// folded to its first line unless everything is shown in full.
    ///
    /// A message with no words yet, or none at all (an empty or blank
    /// body), draws nothing: a bare `✉ from agent …` says nothing, and as
    /// a block of the session it would part the agent's streaming prose.
    /// Its words, when they come with the run that reads it, are a block
    /// then.
    pub fn peer_message(&mut self, run_first: BlockId, from: String, text: String) {
        if text.trim().is_empty() {
            return;
        }
        let waiting = (!text.is_empty())
            .then(|| {
                self.iter()
                    .skip_while(|(id, _)| *id < run_first)
                    .find(|(id, block)| {
                        self.owner_of(*id).is_none()
                            && matches!(block, Block::Peer { from: sender, text, .. }
                                if *sender == from && text.is_empty())
                    })
                    .map(|(id, _)| id)
            })
            .flatten();
        let block = Block::Peer {
            from,
            text,
            collapsed: !self.detail,
            to: None,
        };
        match waiting {
            Some(id) => {
                self.replace_block(id, block);
            }
            None => {
                self.push(block);
            }
        }
    }

    /// Replace a block in place (keeps transcript order). Returns whether `id`
    /// existed.
    pub fn replace_block(&mut self, id: BlockId, block: Block) -> bool {
        match self.block_mut(id) {
            Some(slot) => {
                *slot = block;
                true
            }
            None => false,
        }
    }

    /// Remove a block. Every other id stays as it was; the tool-id index,
    /// the helper headers, the owners and the open streams forget it.
    pub fn remove_block(&mut self, id: BlockId) -> Option<Block> {
        let index = self.index_of(id)?;
        let entry = self.entries.remove(index);
        self.layout.touch(index);
        self.helpers
            .forget_block(id, matches!(entry.block, Block::Helper(_)));
        self.by_tool_id.retain(|_, b| *b != id);
        for entry in &mut self.entries {
            entry.owner = entry.owner.filter(|b| *b != id);
        }
        self.streams.retain(|header, _| *header != Some(id));
        self.held.retain(|header, _| *header != Some(id));
        let deciding: Vec<Option<BlockId>> = self
            .streams
            .iter()
            .filter(|(_, stream)| stream.assistant == Some(id))
            .map(|(scope, _)| *scope)
            .collect();
        for scope in deciding {
            self.held.shift_remove(&scope);
        }
        for stream in self.streams.values_mut() {
            if stream.assistant == Some(id) {
                stream.deciding = false;
            }
            stream.assistant = stream.assistant.filter(|b| *b != id);
            stream.reasoning = stream.reasoning.filter(|b| *b != id);
        }
        Some(entry.block)
    }

    /// Drop every cached layout (e.g. on resize).
    pub fn invalidate_all(&mut self) {
        self.layout.invalidate_all();
        for entry in &mut self.entries {
            entry.forget_layouts();
        }
    }

    /// Ctrl-O: show every foldable block in full, or fold them all back
    /// (one state for the whole transcript, the helpers' blocks and views
    /// included): each tool call's arguments, output and diff, every
    /// reasoning block, each `!` command's output and each job's outcome.
    /// Blocks made while it is on arrive shown. Returns the new state.
    pub fn toggle_detail(&mut self) -> bool {
        self.detail = !self.detail;
        let collapse = !self.detail;
        // Every opened run folds again (with the detail on nothing is in
        // a run).
        if !self.open_runs.is_empty() {
            self.open_runs.clear();
            self.layout.touch(0);
        }
        for index in 0..self.entries.len() {
            let changed = match self.entries[index].block.collapsed_mut() {
                Some(collapsed) => {
                    let changed = *collapsed != collapse;
                    *collapsed = collapse;
                    changed
                }
                None => false,
            };
            if changed {
                self.invalidate(index);
            }
        }
        self.detail
    }

    /// The first laid-out line of block `id`, as of the last
    /// [`Self::ensure_layout`] (`None`: not laid out since it changed).
    #[must_use]
    pub fn start_line(&self, id: BlockId) -> Option<usize> {
        self.layout.start_line(self.index_of(id)?)
    }

    /// The first laid-out line of block `id` and how many lines it takes,
    /// as of the last [`Self::ensure_layout`] (`None`: not laid out since
    /// it changed, or not shown: a folded helper's block takes no line).
    #[must_use]
    pub fn line_span(&self, id: BlockId) -> Option<(usize, usize)> {
        let index = self.index_of(id)?;
        let first = self.start_line(id)?;
        let len = self.lines_of(index);
        (len > 0).then_some((first, len))
    }

    /// Where tool lines show paths from: relative to the project `root`,
    /// with `~` for `home`. The lines already made are made again.
    pub fn set_paths(
        &mut self,
        root: Option<std::path::PathBuf>,
        home: Option<std::path::PathBuf>,
    ) {
        self.paths = tool_line::PathView { root, home };
        self.refresh_tool_heads();
    }

    /// The home directory tool lines show as `~`.
    #[must_use]
    pub fn home(&self) -> Option<&std::path::Path> {
        self.paths.home.as_deref()
    }

    /// Make every tool line's head again (the path view changed, or the
    /// blocks came from a stored conversation).
    fn refresh_tool_heads(&mut self) {
        for index in 0..self.entries.len() {
            if let Block::Tool {
                tool,
                tag,
                args,
                head,
                ..
            } = &self.entries[index].block
            {
                let fresh = tool_line::head(tool, tag.as_deref(), args, &self.paths);
                if fresh != *head
                    && let Some(Entry {
                        block: Block::Tool { head, .. },
                        ..
                    }) = self.invalidate(index)
                {
                    *head = fresh;
                }
            }
        }
    }

    /// Advance the clock of running tool calls (whole seconds). Returns
    /// whether a line changed.
    pub fn tick_tools(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for index in 0..self.entries.len() {
            let Block::Tool {
                state: ToolState::Running { since },
                ticked,
                ..
            } = &self.entries[index].block
            else {
                continue;
            };
            let secs = now.saturating_duration_since(*since).as_secs();
            if secs == *ticked {
                continue;
            }
            if let Some(Entry {
                block: Block::Tool { ticked, .. },
                ..
            }) = self.invalidate(index)
            {
                *ticked = secs;
            }
            changed = true;
        }
        changed
    }

    /// The summary of the latest compaction whose summary is not read yet,
    /// as the conversation's record holds it (`Err`: why it could not be
    /// read). Whether a block took it.
    pub fn fill_compaction(&mut self, read: Result<String, String>) -> bool {
        let Some(index) = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry.block, Block::Compaction { summary: None, .. }))
        else {
            return false;
        };
        if let Some(Entry {
            block: Block::Compaction { summary, .. },
            ..
        }) = self.invalidate(index)
        {
            *summary = Some(read);
        }
        true
    }

    /// Expand or collapse the output of the tool block `id`. Returns the new
    /// `collapsed` state, or `None` when `id` is not a tool block with output.
    pub fn toggle_tool(&mut self, id: BlockId) -> Option<bool> {
        if !self.block(id).is_some_and(foldable) {
            return None;
        }
        let collapsed = self.block_mut(id)?.collapsed_mut()?;
        *collapsed = !*collapsed;
        Some(*collapsed)
    }

    /// The tool block of a call by its tool-use id as the model issued it: a
    /// session call is keyed by that id, a helper's by `{scope}/{id}`.
    fn tool_block_by_call_id(&self, tool_use_id: &str) -> Option<BlockId> {
        self.tool_block(tool_use_id).or_else(|| {
            self.by_tool_id
                .iter()
                .rev()
                .find(|(key, _)| {
                    key.rsplit_once('/')
                        .is_some_and(|(_, call)| call == tool_use_id)
                })
                .map(|(_, id)| *id)
        })
    }

    /// Attach a one-line note to the tool block of `tool_use_id` (a session
    /// call's or a helper's), shown dim under its header. A note for a call
    /// whose block has not arrived yet waits for it.
    pub fn attach_note(&mut self, tool_use_id: &str, text: String) {
        if let Some(id) = self.tool_block_by_call_id(tool_use_id)
            && let Some(Block::Tool { note, .. }) = self.block_mut(id)
        {
            *note = Some(text);
            return;
        }
        if self.pending_notes.len() >= MAX_PENDING_NOTES {
            self.pending_notes.shift_remove_index(0);
        }
        self.pending_notes.insert(tool_use_id.to_string(), text);
    }

    /// Attach a diff to the tool block for `tool_use_id`. Returns whether the block was found.
    pub fn attach_diff(&mut self, tool_use_id: &str, view: DiffView) -> bool {
        let Some(id) = self.tool_block(tool_use_id) else {
            return false;
        };
        if let Some(Block::Tool { diff, .. }) = self.block_mut(id) {
            *diff = Some(view);
            return true;
        }
        false
    }

    /// Ensure every block has a layout at `width`; returns the total line
    /// count. Blocks inside a folded helper lay out to no lines. Only the
    /// blocks from the first one changed since the last call are visited
    /// (all of them when the width changed).
    pub fn ensure_layout(&mut self, width: u16) -> usize {
        // A run's line counts the blocks after its first: a change to one
        // of them lays the run out again from its first block.
        let from = self.layout.stale_from(width, self.entries.len());
        self.layout.touch(self.run_start(from));
        let start = self.layout.begin(width, self.entries.len());
        let roles = self.transcript_roles(start);
        for (i, role) in (start..self.entries.len()).zip(roles) {
            let entry = &self.entries[i];
            if entry
                .layout
                .as_ref()
                .is_none_or(|cached| cached.width != width)
                || entry.role != role
            {
                let lines = if self.is_hidden(entry.id) {
                    Vec::new()
                } else {
                    role_lines(&entry.block, &role, width)
                };
                #[cfg(test)]
                self.layout.note_laid_out();
                self.entries[i].layout = Some(CachedLayout { width, lines });
                self.entries[i].role = role;
            }
            let lines = self.lines_of(i);
            self.layout.record(i, lines);
        }
        self.layout.finish(self.entries.len())
    }

    /// The cached line count of block `index` (0 when not laid out).
    fn lines_of(&self, index: usize) -> usize {
        self.layout_at(index).map_or(0, |c| c.lines.len())
    }

    /// The cached layout of the block at position `index`.
    fn layout_at(&self, index: usize) -> Option<&CachedLayout> {
        self.entries.get(index)?.layout.as_ref()
    }

    /// The block holding laid-out line `idx` and the line's offset in it,
    /// as of the last [`Self::ensure_layout`]: a binary search over the
    /// clean prefix, then a walk over the blocks changed since.
    fn locate(&self, idx: usize) -> Option<(usize, usize)> {
        let known = self.layout.known(self.entries.len());
        let from = if idx < known.lines {
            self.layout.block_holding(idx, known.blocks)
        } else {
            known
        };
        let (mut block, mut base) = (from.blocks, from.lines);
        while block < self.entries.len() {
            #[cfg(test)]
            self.layout.note_walked();
            let n = self.lines_of(block);
            if idx < base + n {
                return Some((block, idx - base));
            }
            base += n;
            block += 1;
        }
        None
    }

    /// The laid-out lines `[start, start + len)` with the block each is
    /// in, from the first one's block on.
    fn visible(&self, start: usize, len: usize) -> Vec<(BlockId, &Line<'static>)> {
        let mut out = Vec::with_capacity(len);
        let Some((mut block, mut skip)) = self.locate(start) else {
            return out;
        };
        while out.len() < len && block < self.entries.len() {
            #[cfg(test)]
            self.layout.note_walked();
            let entry = &self.entries[block];
            if let Some(cached) = &entry.layout {
                let room = len - out.len();
                out.extend(
                    cached
                        .lines
                        .iter()
                        .skip(skip)
                        .take(room)
                        .map(|line| (entry.id, line)),
                );
            }
            skip = 0;
            block += 1;
        }
        out
    }

    /// Lines `[start, start + len)` of the laid-out transcript. Call [`Self::ensure_layout`] first.
    pub fn visible_lines(&self, start: usize, len: usize) -> Vec<Line<'static>> {
        self.visible(start, len)
            .into_iter()
            .map(|(_, line)| line.clone())
            .collect()
    }

    /// Number of laid-out lines as of the last [`Self::ensure_layout`].
    pub fn line_count(&self) -> usize {
        let known = self.layout.known(self.entries.len());
        known.lines
            + (known.blocks..self.entries.len())
                .map(|i| self.lines_of(i))
                .sum::<usize>()
    }

    /// The block drawn on each of the lines `[start, start + len)`, matching
    /// [`Self::visible_lines`]. Call [`Self::ensure_layout`] first.
    pub fn visible_blocks(&self, start: usize, len: usize) -> Vec<BlockId> {
        self.visible(start, len)
            .into_iter()
            .map(|(block, _)| block)
            .collect()
    }

    /// Apply a primary-agent event at `now`. Returns the usage of a
    /// completed LLM turn, if any (a turn the provider sent no accounting
    /// for returns `None`).
    pub fn apply_agent_event_at(&mut self, event: &AgentEvent, now: Instant) -> Option<TurnUsage> {
        self.apply_event(None, event, now)
    }

    /// Apply one event of the session (`helper` is `None`) or of a helper.
    /// Streaming and the tool lifecycle are the same for both; a helper's
    /// blocks belong to its header and its run updates the header, and
    /// only the session's own events become notices. Returns the usage of
    /// a completed model call (a call the provider sent no accounting for
    /// returns `None`).
    fn apply_event(
        &mut self,
        helper: Option<&HelperScope<'_>>,
        event: &AgentEvent,
        now: Instant,
    ) -> Option<TurnUsage> {
        let scope = helper.map(|h| h.header);
        let key = |id: &str| match helper {
            Some(h) => format!("{}/{id}", h.scope_id),
            None => id.to_string(),
        };
        if let AgentEvent::ToolCallRequested { name, args, .. } = event {
            self.note_spawn(name, args.as_value());
        }
        match event {
            AgentEvent::TurnStarted { .. } => {
                self.settle_held(scope);
                *self.stream(scope) = Stream::default();
            }
            AgentEvent::TurnCompleted { usage, .. } => {
                self.close_streams(scope);
                if let (Some(h), Some(usage)) = (helper, usage) {
                    let tokens = usage.normalized_total_tokens();
                    self.update_fold(h.header, |f| {
                        f.usage = Some(f.usage.unwrap_or(0).saturating_add(tokens));
                    });
                }
                return usage.clone();
            }
            AgentEvent::TextDelta { delta, .. } => self.append_text(scope, delta),
            AgentEvent::TextComplete { content, .. } => {
                let stream = *self.stream(scope);
                if stream.assistant.is_none() && !stream.text_seen && !content.is_empty() {
                    self.append_text(scope, content);
                }
                if let Some(h) = helper
                    && let Some(line) = content.lines().find(|l| !l.trim().is_empty())
                {
                    let line = truncate_chars(line.trim(), MAX_HELPER_PREVIEW);
                    self.update_fold(h.header, |f| f.last = HelperLast::Text(line));
                }
            }
            AgentEvent::ReasoningDelta { delta, .. } => self.append_reasoning(scope, delta),
            AgentEvent::ReasoningComplete { content, .. } => {
                let stream = *self.stream(scope);
                if stream.reasoning.is_none()
                    && !stream.reasoning_seen
                    && !content.trim().is_empty()
                {
                    self.append_reasoning(scope, content);
                }
                self.stream(scope).reasoning = None;
            }
            AgentEvent::ToolCallRequested { id, name, args } => match helper {
                None => {
                    self.request_tool(
                        None,
                        id.clone(),
                        name.clone(),
                        None,
                        args.as_value().clone(),
                        0,
                    );
                }
                Some(h) => {
                    let preview = helper_args_preview(args.as_value());
                    self.update_fold(h.header, |f| {
                        f.calls += 1;
                        f.last = HelperLast::Tool {
                            name: name.clone(),
                            args: preview,
                        };
                    });
                    self.request_tool(
                        scope,
                        key(id),
                        name.clone(),
                        Some(h.tag.clone()),
                        args.as_value().clone(),
                        h.depth,
                    );
                }
            },
            AgentEvent::ToolExecutionStarted { id, .. } => self.start_tool(&key(id), now),
            AgentEvent::ToolExecutionCompleted {
                id,
                is_error,
                duration_ms,
                ..
            } => self.complete_tool(&key(id), *is_error, *duration_ms, None),
            // The result body arrives on its own event, after the execution fact.
            AgentEvent::ToolResultReceived { id, content, .. } => {
                self.tool_result(&key(id), &text_content(content));
            }
            AgentEvent::ToolExecutionTimedOut { id, timeout_ms, .. } => {
                let text = format!("timed out after {timeout_ms}ms");
                self.complete_tool(&key(id), true, *timeout_ms, Some(&text));
                let after = tool_line::human_secs(*timeout_ms as f64 / 1_000.0);
                if let Some(block) = self.tool_block(&key(id))
                    && let Some(Block::Tool { outcome, .. }) = self.block_mut(block)
                {
                    *outcome = tool_line::Outcome::failure(format!("timed out after {after}"));
                }
            }
            // A run at its limit is at its checkpoint, no failure: the
            // daemon's line says what follows (`crate::daemon::checkpoint`).
            AgentEvent::RunFailed { error_report, .. }
                if error_report.class == meerkat_core::AgentErrorClass::MaxTurns => {}
            // The agent's own cancelled run: an interrupt, which the turn's
            // end says in its one line (never the engine's "Run was
            // cancelled" as an error).
            AgentEvent::RunFailed { error_report, .. }
                if helper.is_none()
                    && error_report.class == meerkat_core::AgentErrorClass::Cancelled => {}
            AgentEvent::RunFailed { error_report, .. } => match helper {
                None => {
                    self.push_notice(
                        NoticeLevel::Error,
                        format!("error: {}", error_report.message),
                    );
                }
                Some(h) => {
                    let cancelled = error_report.class == meerkat_core::AgentErrorClass::Cancelled;
                    let state = if cancelled {
                        HelperState::Cancelled
                    } else {
                        HelperState::Failed
                    };
                    self.update_fold(h.header, |f| f.state = state);
                    // A cancelled run's words are the engine's; how the
                    // helper ended comes from the daemon (`helper_ended`).
                    if !cancelled {
                        self.push_scoped(
                            scope,
                            Block::Notice {
                                level: NoticeLevel::Error,
                                text: format!(
                                    "{} error: {}",
                                    h.tag,
                                    crate::mobs::endings::plain_reason(&error_report.message)
                                ),
                            },
                        );
                    }
                }
            },
            AgentEvent::RunStarted { input, .. } if let Some(h) = helper => {
                self.helper_input(h, input.prompt_text().as_deref());
                self.update_fold(h.header, |f| {
                    if matches!(
                        f.state,
                        HelperState::Done | HelperState::Failed | HelperState::Cancelled
                    ) {
                        // A seated helper's later run (a message woke it):
                        // the same header, running again.
                        f.state = HelperState::Running;
                        f.ended = None;
                    }
                });
            }
            AgentEvent::RunCompleted { .. } if let Some(h) = helper => {
                self.update_fold(h.header, |f| {
                    if f.state.is_running() {
                        f.state = HelperState::Done;
                    }
                });
            }
            // A helper's other events are not shown.
            _ if helper.is_some() => {}
            AgentEvent::HookDenied {
                hook_id,
                point,
                message,
                payload,
                ..
            } => self.hook_denied(&hook_id.0, *point, message, payload.as_ref()),
            AgentEvent::HookFailed {
                hook_id, reason, ..
            } => {
                self.push_notice(
                    NoticeLevel::Warn,
                    format!("hook failed: {hook_id}: {reason}"),
                );
            }
            AgentEvent::HookLaunchRefused {
                hook_id, reason, ..
            } => {
                self.push_notice(
                    NoticeLevel::Warn,
                    format!("hook refused to launch: {hook_id}: {reason}"),
                );
            }
            AgentEvent::Retrying { retry, .. } => {
                self.push_notice(NoticeLevel::Warn, retry_notice(retry));
            }
            AgentEvent::BudgetWarning {
                budget_type,
                used,
                limit,
                percent,
            } => {
                let what = match budget_type {
                    meerkat_core::BudgetType::Tokens => "the token budget",
                    meerkat_core::BudgetType::Time => "the time budget",
                    meerkat_core::BudgetType::ToolCalls => "the tool-call budget",
                };
                self.push_notice(
                    NoticeLevel::Warn,
                    format!(
                        "⚠ {what} is {:.0}% used ({used} of {limit})",
                        percent * 100.0
                    ),
                );
            }
            AgentEvent::CompactionStarted {
                message_count,
                estimated_history_tokens,
                ..
            } => {
                self.push_notice(
                    NoticeLevel::Info,
                    format!(
                        "⟳ compacting context ({message_count} messages, ~{estimated_history_tokens} tokens)…"
                    ),
                );
            }
            AgentEvent::CompactionCompleted {
                summary_tokens,
                messages_before,
                messages_after,
                ..
            } => {
                // The summary's text is not in the event: the client reads
                // it from the conversation's record (`fill_compaction`).
                let collapsed = !self.detail;
                self.push(Block::Compaction {
                    line: format!(
                        "compacted: {messages_before} → {messages_after} messages ({} summary tokens)",
                        crate::text::tokens(*summary_tokens)
                    ),
                    summary: None,
                    collapsed,
                });
            }
            AgentEvent::CompactionFailed { reason } => {
                self.push_notice(NoticeLevel::Warn, format!("✗ compaction failed: {reason}"));
            }
            AgentEvent::RequestedCompactionSkipped { reason, .. } => {
                self.push_notice(NoticeLevel::Info, compaction_skipped(*reason));
            }
            AgentEvent::StreamTruncated { reason } => {
                self.push_notice(
                    NoticeLevel::Warn,
                    format!("some stream events were dropped: {reason}"),
                );
            }
            AgentEvent::SkillResolutionFailed { skill_key, reason } => {
                let text = match skill_key {
                    Some(key) => format!("⚠ skill failed: {key}: {reason}"),
                    None => format!("⚠ skill failed: {reason}"),
                };
                self.push_notice(NoticeLevel::Warn, text);
            }
            AgentEvent::BackgroundJobCompleted {
                job_id,
                display_name,
                detail,
                ..
            } if let Some(outcome) =
                outcome::JobOutcome::from_detail(display_name, Some(job_id), detail) =>
            {
                self.push_outcome(&outcome);
            }
            other => {
                if let Some((level, text)) = upstream_notice(other) {
                    self.push_notice(level, text);
                }
            }
        }
        None
    }

    /// `outcome`'s block, unless the transcript shows that job's already
    /// (its event and the run it starts both carry it).
    pub fn push_outcome(&mut self, outcome: &outcome::JobOutcome) {
        let shown = outcome.job.is_some()
            && self.entries.iter().any(
                |entry| matches!(&entry.block, Block::Outcome { job, .. } if *job == outcome.job),
            );
        if !shown {
            self.push(outcome.block());
        }
    }

    fn push_raw(&mut self, mut block: Block) -> BlockId {
        // With the detail on (Ctrl-O), a foldable block arrives shown.
        if self.detail
            && let Some(collapsed) = block.collapsed_mut()
        {
            *collapsed = false;
        }
        let id = BlockId(self.next_id);
        self.next_id += 1;
        if matches!(block, Block::Helper(_)) {
            self.helpers.count_header();
        }
        self.entries.push(Entry {
            id,
            block,
            layout: None,
            view_layout: None,
            role: RunRole::Alone,
            view_role: RunRole::Alone,
            owner: None,
        });
        id
    }

    /// The streaming state of `scope` (the session's when `None`, else
    /// the helper whose header it is), made when missing.
    fn stream(&mut self, scope: Option<BlockId>) -> &mut Stream {
        self.streams.entry(scope).or_default()
    }

    /// Append prose to `scope`'s open prose block (made, owned by the
    /// helper's header, when none is open); its reasoning block closes.
    /// An empty delta changes nothing.
    ///
    /// A new prose block decides its first line first ([`leaked_thought`]):
    /// a start of the line `thought` is held, not shown; the line whole
    /// makes the block a collapsed reasoning block, which takes the
    /// step's text from then on; anything else is prose. Blank space goes
    /// into the block at once (it shows nothing).
    fn append_text(&mut self, scope: Option<BlockId>, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let stream = *self.stream(scope);
        if stream.thought
            && let Some(id) = stream.reasoning
            && let Some(Block::Reasoning { text, .. }) = self.block_mut(id)
        {
            text.push_str(delta);
            self.stream(scope).text_seen = true;
            return;
        }
        let id = match stream.assistant {
            Some(id) if matches!(self.block(id), Some(Block::Assistant { .. })) => id,
            _ => {
                let id = self.push_raw(Block::Assistant { md: String::new() });
                self.set_owner(id, scope);
                let stream = self.stream(scope);
                stream.assistant = Some(id);
                stream.deciding = true;
                id
            }
        };
        if self.stream(scope).deciding {
            let held = self.held.shift_remove(&scope).unwrap_or_default();
            let md = match self.block(id) {
                Some(Block::Assistant { md }) => md.clone(),
                _ => String::new(),
            };
            let whole = format!("{md}{held}{delta}");
            match leaked_thought(&whole) {
                Thought::Is(reasoning) => {
                    let collapsed = !self.detail;
                    self.replace_block(
                        id,
                        Block::Reasoning {
                            text: reasoning.to_string(),
                            collapsed,
                        },
                    );
                    let stream = self.stream(scope);
                    stream.assistant = None;
                    stream.reasoning = Some(id);
                    stream.thought = true;
                    stream.deciding = false;
                    stream.text_seen = true;
                    return;
                }
                Thought::Undecided => {
                    let words = whole.trim_start();
                    let blank = whole[..whole.len() - words.len()].to_string();
                    if !words.is_empty() {
                        self.held.insert(scope, words.to_string());
                    }
                    if let Some(Block::Assistant { md }) = self.block_mut(id) {
                        *md = blank;
                    }
                }
                Thought::Not => {
                    if let Some(Block::Assistant { md }) = self.block_mut(id) {
                        *md = whole;
                    }
                    self.stream(scope).deciding = false;
                }
            }
        } else if let Some(Block::Assistant { md }) = self.block_mut(id) {
            md.push_str(delta);
        }
        let stream = self.stream(scope);
        stream.reasoning = None;
        stream.text_seen = true;
    }

    /// The prose `scope` closes or something else of it comes: a first
    /// line it still held was not the line `thought`, and shows as the
    /// prose it is.
    fn settle_held(&mut self, scope: Option<BlockId>) {
        let Some(stream) = self.streams.get_mut(&scope) else {
            return;
        };
        if !std::mem::take(&mut stream.deciding) {
            return;
        }
        let assistant = stream.assistant;
        let held = self.held.shift_remove(&scope).unwrap_or_default();
        if let Some(id) = assistant
            && !held.is_empty()
            && let Some(Block::Assistant { md }) = self.block_mut(id)
        {
            md.push_str(&held);
        }
    }

    /// Append reasoning to `scope`'s open reasoning block (made collapsed,
    /// owned by the helper's header, when none is open); its prose block
    /// closes. Blank reasoning with no reasoning block open is dropped: it
    /// shows nothing, so it neither opens a block nor parts the prose.
    fn append_reasoning(&mut self, scope: Option<BlockId>, delta: &str) {
        self.settle_held(scope);
        let stream = *self.stream(scope);
        if stream.reasoning.is_none() && delta.trim().is_empty() {
            return;
        }
        if let Some(id) = stream.reasoning
            && let Some(Block::Reasoning { text, .. }) = self.block_mut(id)
        {
            text.push_str(delta);
        } else {
            let id = self.push_raw(Block::Reasoning {
                text: delta.to_string(),
                collapsed: true,
            });
            self.set_owner(id, scope);
            self.stream(scope).reasoning = Some(id);
        }
        let stream = self.stream(scope);
        stream.assistant = None;
        stream.thought = false;
        stream.reasoning_seen = true;
    }

    /// Close `scope`'s open prose and reasoning blocks.
    fn close_streams(&mut self, scope: Option<BlockId>) {
        self.settle_held(scope);
        if let Some(stream) = self.streams.get_mut(&scope) {
            stream.assistant = None;
            stream.reasoning = None;
            stream.thought = false;
        }
    }

    /// A tool call of `scope` (the session's, or a helper's under its
    /// header, `tag` its tag): it closes that scope's prose only.
    fn request_tool(
        &mut self,
        scope: Option<BlockId>,
        key: String,
        tool: String,
        tag: Option<String>,
        args: Value,
        depth: usize,
    ) -> BlockId {
        let note = self.pending_notes.shift_remove(&key).or_else(|| {
            key.rsplit_once('/')
                .and_then(|(_, call)| self.pending_notes.shift_remove(call))
        });
        let args = self.with_peer_name(&tool, args);
        let head = tool_line::head(&tool, tag.as_deref(), &args, &self.paths);
        let id = self.push_scoped(
            scope,
            Block::Tool {
                tool,
                tag,
                args,
                head,
                outcome: tool_line::Outcome::default(),
                ticked: 0,
                state: ToolState::Requested,
                result_preview: None,
                diff: None,
                note,
                depth,
                collapsed: true,
            },
        );
        self.by_tool_id.insert(key, id);
        id
    }

    /// A `send_message`'s arguments with the recipient's name the
    /// transcript knows (`display_name`, which the tool itself only logs),
    /// for its header; other calls' unchanged.
    fn with_peer_name(&self, tool: &str, mut args: Value) -> Value {
        if tool == SEND_TOOL
            && args.get("display_name").is_none_or(Value::is_null)
            && let Some(known) = args
                .get("peer_id")
                .and_then(Value::as_str)
                .and_then(|peer| self.peer_names.get(peer))
            && let Some(obj) = args.as_object_mut()
        {
            obj.insert("display_name".to_string(), Value::String(known.clone()));
        }
        args
    }

    /// What a finished call's result tells the transcript: a `contacts`
    /// result names the peers; a `send_message` that was sent folds to
    /// `sent to <name>` (its receipt behind the toggle), one that failed
    /// shows its error unfolded. `whole` is the whole result (never the
    /// preview, which a longer result is cut to and which then does not
    /// parse), when it has come.
    fn learn_from_result(&mut self, id: BlockId, whole: Option<&str>) {
        let Some(Block::Tool {
            tool, args, state, ..
        }) = self.block(id)
        else {
            return;
        };
        let result = whole.and_then(|text| serde_json::from_str::<Value>(text).ok());
        // `contacts` (and an earlier build's `peers`): every entry naming a
        // peer id, in whichever group it is.
        if matches!(tool.as_str(), "peers" | crate::daemon::contacts::CONTACTS) {
            fn entries(value: &Value, out: &mut Vec<Value>) {
                match value {
                    Value::Object(map) => {
                        if map.contains_key("peer_id") {
                            out.push(value.clone());
                        }
                        map.values().for_each(|v| entries(v, out));
                    }
                    Value::Array(items) => items.iter().for_each(|v| entries(v, out)),
                    _ => {}
                }
            }
            let mut peers = Vec::new();
            if let Some(result) = &result {
                entries(result, &mut peers);
            }
            for peer in peers {
                if let (Some(id), Some(name)) = (
                    peer.get("peer_id").and_then(Value::as_str),
                    peer.get("name").and_then(Value::as_str),
                ) {
                    self.peer_names.insert(id.to_string(), name.to_string());
                }
            }
            return;
        }
        if tool != SEND_TOOL {
            return;
        }
        let failed = matches!(state, ToolState::Done { is_error: true, .. });
        let sent = result
            .as_ref()
            .and_then(|r| r.get("status"))
            .and_then(Value::as_str)
            == Some("sent");
        let recipient = tool_line::recipient(args).to_string();
        if let Some(Block::Tool {
            note, collapsed, ..
        }) = self.block_mut(id)
        {
            if failed {
                *collapsed = false;
            } else if sent && note.is_none() {
                *note = Some(format!("sent to {recipient}"));
            }
        }
    }

    fn start_tool(&mut self, key: &str, now: Instant) {
        let Some(id) = self.tool_block(key) else {
            return;
        };
        if let Some(Block::Tool { state, .. }) = self.block_mut(id) {
            *state = ToolState::Running { since: now };
        }
    }

    fn complete_tool(&mut self, key: &str, is_error: bool, duration_ms: u64, result: Option<&str>) {
        let Some(id) = self.tool_block(key) else {
            return;
        };
        if let Some(Block::Tool {
            state,
            result_preview,
            ..
        }) = self.block_mut(id)
        {
            *state = ToolState::Done {
                is_error,
                duration_ms,
            };
            if let Some(result) = result {
                *result_preview = Some(result).filter(|r| !r.is_empty()).map(Preview::of);
            }
            if let (
                Some(text),
                Some(Block::Tool {
                    tool,
                    args,
                    state,
                    outcome,
                    ..
                }),
            ) = (result, self.block_mut(id))
            {
                *outcome = Self::outcome_of(tool, args, state, text);
            }
        }
        self.learn_from_result(id, result);
    }

    /// A finished call's outcome from its whole `result`: the tool's row
    /// ([`tool_line::outcome`]), and for a call that reported an error
    /// without one, the result's first line as the reason.
    fn outcome_of(tool: &str, args: &Value, state: &ToolState, result: &str) -> tool_line::Outcome {
        let mut outcome = tool_line::outcome(tool, args, result);
        if matches!(state, ToolState::Done { is_error: true, .. }) && outcome.text.is_none() {
            outcome.text = tool_line::failure_reason(result);
        }
        outcome
    }

    /// Attach a tool's result body (it arrives after the execution fact).
    fn tool_result(&mut self, key: &str, result: &str) {
        let Some(id) = self.tool_block(key) else {
            return;
        };
        if let Some(Block::Tool {
            tool,
            args,
            state,
            outcome,
            result_preview,
            ..
        }) = self.block_mut(id)
        {
            *result_preview = Some(result).filter(|r| !r.is_empty()).map(Preview::of);
            *outcome = Self::outcome_of(tool, args, state, result);
        }
        self.learn_from_result(id, Some(result));
    }

    fn hook_denied(
        &mut self,
        hook_id: &str,
        point: HookPoint,
        message: &str,
        payload: Option<&Value>,
    ) {
        if point != HookPoint::PreToolExecution {
            self.push_notice(
                NoticeLevel::Warn,
                format!("⊘ hook denied ({point:?}): {hook_id}: {message}"),
            );
            return;
        }
        let payload_str = |key: &str| {
            payload
                .and_then(|p| p.get(key))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let target = payload_str("tool_use_id")
            .and_then(|tid| self.tool_block(&tid))
            .or_else(|| {
                self.by_tool_id.values().copied().find(|id| {
                    matches!(
                        self.block(*id),
                        Some(Block::Tool {
                            state: ToolState::Requested,
                            ..
                        })
                    )
                })
            });
        let mut tool = payload_str("tool").unwrap_or_default();
        if let Some(id) = target
            && let Some(Block::Tool {
                tool: called,
                tag,
                state,
                ..
            }) = self.block_mut(id)
        {
            *state = ToolState::Denied {
                reason: message.to_string(),
            };
            if tool.is_empty() {
                // A helper's call as its line shows it: its tag in front.
                tool = match tag {
                    Some(tag) => format!("{tag} {called}"),
                    None => called.clone(),
                };
            }
        }
        let tier = payload_str("tier").unwrap_or_else(|| hook_id.to_string());
        self.push(Block::Approval {
            tool,
            tier,
            allowed: false,
            reason: Some(message.to_string()),
        });
    }
}

/// The comms tool that sends a peer a message.
const SEND_TOOL: &str = "send_message";

/// Which of its two reports showed a run nobody here typed
/// ([`Transcript::remote_input`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Heard {
    /// The daemon's note of the run (`rkat/triggered`).
    Triggered,
    /// The run's own input (its start event).
    Input,
}

/// Most runs [`Transcript::remote_input`] remembers as shown by one report
/// only.
const MAX_ANNOUNCED: usize = 16;

/// The one dim line a run's input is drawn as when the daemon or this
/// client composed it, never as the user's band or the pinned line
/// (`None`: not one of theirs). Live, each of these has its own notice or
/// triggered line already and the input shows nothing more; a restored
/// conversation shows this line in its place. A helper's ending note,
/// held notes and `/compact`'s prompt are handled where they are read; a
/// schedule's prompt was written by the user or the agent and stays its
/// triggered line live.
#[must_use]
pub fn composed_input(text: &str) -> Option<&'static str> {
    use crate::daemon::{checkpoint, prepare};
    let text = text.trim();
    let prepare_request = text.ends_with(prepare::READY_SUFFIX)
        || ((text.starts_with(prepare::BACKSTOP_OPENING)
            || text.starts_with(prepare::CONTEXT_OPENING))
            && text.ends_with(prepare::REMINDER_CLOSING));
    if prepare_request {
        Some(prepare::PREPARE_LABEL)
    } else if text == checkpoint::CONTINUE_INSTRUCTION {
        Some(checkpoint::CONTINUE_LABEL)
    } else if text.starts_with(checkpoint::STATUS_OPENING) {
        Some(checkpoint::STATUS_LABEL)
    } else if text == crate::approval::mode::PLAN_ON_NOTE {
        Some("plan mode on: the agent was told")
    } else if text == crate::approval::mode::PLAN_OFF_NOTE {
        Some("plan mode off: the agent was told")
    } else if text.starts_with(crate::approval::engine::APPROVED_OPENING) {
        Some(crate::daemon::agents::APPROVED_LABEL)
    } else if text.starts_with(crate::daemon::watches::FILES_CHANGED) {
        Some("files changed: the agent was told")
    } else if text == crate::daemon::agents::READ_ONLY_COMMS_NOTE {
        Some("the agent was told that a read-only profile cannot send messages")
    } else if text == crate::ui::COMPACT_PROMPT {
        Some("compacted")
    } else {
        None
    }
}

/// `text` split into the held notes and the user's words that follow
/// them, when it is a message the daemon put held notes before
/// (`crate::daemon::local::HELD_NOTE_SEPARATOR`).
pub(super) fn split_held(text: &str) -> Option<(&str, &str)> {
    let (held, words) = text.split_once(crate::daemon::local::HELD_NOTE_SEPARATOR)?;
    Some((held.trim(), words.trim()))
}

/// The folded [`Block::HelperNote`] for a run the daemon started with a
/// note of helpers' endings (`label` is one of its helper labels): what
/// the note reads back as, or, for words this build does not know, the
/// label and the note's first line.
pub(crate) fn helper_note(
    label: &str,
    prompt: &str,
    summary: Option<crate::daemon::agents::endings::NoteSummary>,
    collapsed: bool,
) -> Option<Block> {
    use crate::daemon::agents::{HELPER_ENDED_LABEL, HELPER_IDLE_LABEL, endings};
    if label != HELPER_ENDED_LABEL && label != HELPER_IDLE_LABEL {
        return None;
    }
    // The daemon's own, made from the endings; else the note's header.
    let summary = summary
        .or_else(|| endings::note_summary(prompt))
        .unwrap_or_else(|| endings::NoteSummary {
            title: label.to_string(),
            first: prompt
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .trim()
                .to_string(),
            helpers: Vec::new(),
            failed: false,
            kind: endings::NoteKind::Several,
        });
    Some(Block::HelperNote {
        title: summary.title,
        first: summary.first,
        text: prompt.to_string(),
        helpers: summary.helpers,
        failed: summary.failed,
        kind: summary.kind,
        collapsed,
        paired: false,
    })
}

impl Block {
    /// Whether only its first line shows, for the kinds that fold (a tool
    /// call, reasoning, a `!` command, a peer's message, a helper's note,
    /// a compaction, a job's outcome); `None` for the others.
    fn collapsed_mut(&mut self) -> Option<&mut bool> {
        match self {
            Self::Reasoning { collapsed, .. }
            | Self::Tool { collapsed, .. }
            | Self::Shell { collapsed, .. }
            | Self::Peer { collapsed, .. }
            | Self::HelperNote { collapsed, .. }
            | Self::Compaction { collapsed, .. }
            | Self::Outcome { collapsed, .. } => Some(collapsed),
            _ => None,
        }
    }
}

/// Whether `block` has something to fold (its line ends in `▸`/`▾`): a
/// tool call (its arguments, output, diff or reason), a reasoning block, a
/// `!` command's output or a job's outcome.
#[must_use]
pub fn foldable(block: &Block) -> bool {
    matches!(block, Block::Tool { args, result_preview, diff, state, outcome, .. }
        if result_preview.is_some()
            || diff.is_some()
            || args.as_object().is_some_and(|map| !map.is_empty())
            || matches!(state, ToolState::Denied { .. })
            || outcome.text.is_some())
        || matches!(block, Block::Reasoning { text, .. } if !text.is_empty())
        || matches!(
            block,
            Block::Shell {
                output: Some(_),
                ..
            }
        )
        || matches!(block, Block::Outcome { md, .. } if !md.is_empty())
        || matches!(block, Block::Peer { text, .. } if !text.is_empty())
        || matches!(block, Block::HelperNote { .. } | Block::Compaction { .. })
}

/// What an opened [`Block::Compaction`] shows below its line, and the
/// export writes under its heading: the summary, or where it is.
#[must_use]
pub fn compaction_body(summary: Option<&Result<String, String>>) -> String {
    match summary {
        Some(Ok(text)) => text.clone(),
        Some(Err(why)) => format!(
            "(the summary could not be read: {why}. It is the first message of the conversation's record, and this line shows it when the conversation is opened again, with /resume)"
        ),
        None => "(reading the summary from the conversation's record…)".to_string(),
    }
}

/// The text of a stored compaction summary (`[Context compacted] …`)
/// without the engine's lead-in.
#[must_use]
pub fn compaction_summary_text(text: &str) -> &str {
    text.strip_prefix(meerkat_core::COMPACTION_SUMMARY_PREFIX)
        .unwrap_or(text)
        .trim()
}

/// The text of the latest compaction summary in a conversation's
/// committed `messages`, without its lead-in (`None`: none there).
#[must_use]
pub fn latest_compaction_summary(messages: &[meerkat_core::Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| match message {
        meerkat_core::Message::User(user) if user.transcript_role.is_compaction_summary() => {
            Some(compaction_summary_text(&user.text_content()).to_string())
        }
        _ => None,
    })
}

/// A `!` command's state as the export and the header say it.
pub fn shell_state_text(state: &ShellState) -> String {
    match state {
        ShellState::Running { .. } => "running".to_string(),
        ShellState::Done { exit, duration_ms } => format!("{exit}, {duration_ms} ms"),
        ShellState::Cancelled => "cancelled".to_string(),
        ShellState::Failed(reason) => format!("could not run: {reason}"),
    }
}

/// A `!` command's output as its block shows it: capped like a tool
/// result, `None` when there is none.
#[must_use]
pub fn shell_output_preview(output: &str) -> Option<Preview> {
    let output = output.trim_end_matches('\n');
    (!output.is_empty()).then(|| Preview::of(output))
}

/// Event builders shared by the UI unit tests.
#[cfg(test)]
pub(crate) mod test_support {
    use meerkat_core::types::ContentBlock;
    use meerkat_core::{AgentEvent, Provider, ToolCallArguments, TurnUsage, Usage};
    use serde_json::Value;

    /// Tool-call arguments from a JSON object literal.
    #[allow(clippy::expect_used)]
    pub(crate) fn test_args(args: Value) -> ToolCallArguments {
        ToolCallArguments::from_value(args).expect("tool-call arguments are a JSON object")
    }

    /// The body of a finished tool call, as upstream publishes it after
    /// `ToolExecutionCompleted`.
    pub(crate) fn test_tool_result(id: &str, text: &str) -> AgentEvent {
        AgentEvent::ToolResultReceived {
            id: id.into(),
            name: String::new(),
            content: vec![ContentBlock::Text { text: text.into() }],
            is_error: false,
        }
    }

    /// Provider-accounted usage for a `TurnCompleted` event.
    pub(crate) fn test_usage(usage: Usage) -> Option<TurnUsage> {
        Some(TurnUsage::host_declared(Provider::Anthropic, "test", usage))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::test_support::{test_args, test_tool_result, test_usage};
    use super::*;
    use crate::ui::render::block_lines;
    use meerkat_core::{LlmRetrySchedule, ScopedAgentEvent, StopReason, StreamScopeFrame, Usage};
    use serde_json::json;

    fn text(d: &str) -> AgentEvent {
        AgentEvent::TextDelta {
            delta: d.to_string(),
            assistant_message_id: None,
        }
    }

    /// A fired run's prompt, shown as the user's when its start arrived,
    /// is relabelled with what fired it; one not in the run is appended.
    #[test]
    fn a_triggered_run_relabels_its_prompt() {
        let mut t = Transcript::default();
        t.push(Block::User {
            text: "check the build".into(),
        });
        let run = t.next_id();
        t.push(Block::User {
            text: "check the build".into(),
        });
        use crate::daemon::protocol::TriggerCause;
        t.label_triggered(
            run,
            (TriggerCause::Schedule, "scheduled: nightly".into()),
            "check the build".into(),
            None,
        );
        assert!(matches!(&t.blocks()[0], Block::User { .. }));
        assert!(
            matches!(&t.blocks()[1], Block::Triggered { label, text, .. }
            if label == "scheduled: nightly" && text == "check the build")
        );
        t.label_triggered(
            t.next_id(),
            (TriggerCause::Schedule, "scheduled: other".into()),
            "elsewhere".into(),
            None,
        );
        assert_eq!(t.len(), 3);
        assert!(
            matches!(&t.blocks()[2], Block::Triggered { label, .. } if label == "scheduled: other")
        );
    }

    #[test]
    fn export_text_has_a_header_per_block_and_folds_collapsed_output() {
        let mut t = Transcript::new();
        t.push(Block::User {
            text: "fix it".into(),
        });
        t.push(Block::Reasoning {
            text: "secret thoughts".into(),
            collapsed: true,
        });
        let tool = |collapsed| Block::Tool {
            tool: "read_file".into(),
            tag: None,
            head: tool_line::head(
                "read_file",
                None,
                &json!({"path": "a.rs"}),
                &tool_line::PathView::default(),
            ),
            outcome: tool_line::Outcome::default(),
            ticked: 0,
            args: json!({"path": "a.rs"}),
            state: ToolState::Done {
                is_error: false,
                duration_ms: 3,
            },
            result_preview: Some("fn main() {}".into()),
            diff: None,
            note: Some("allowed by rule".into()),
            depth: 0,
            collapsed,
        };
        t.push(tool(true));
        t.push(tool(false));
        t.push(Block::Assistant {
            md: "done\n".into(),
        });
        t.push(Block::Approval {
            tool: "shell".into(),
            tier: "classifier".into(),
            allowed: false,
            reason: Some("rm -rf".into()),
        });
        t.push_notice(NoticeLevel::Warn, "turn interrupted");
        let text = t.export_text();
        assert_eq!(
            text,
            "## user\nfix it\n\n## reasoning\n\n\
             ## tool ✓ read a.rs\nallowed by rule\n\n\
             ## tool ✓ read a.rs\nallowed by rule\n{\n  \"path\": \"a.rs\"\n}\nfn main() {}\n\n\
             ## assistant\ndone\n\n\
             ## approval: shell denied by classifier: rm -rf\n\n\
             ## notice (warning)\nturn interrupted\n"
        );
        assert!(!text.contains("secret thoughts"));
    }

    /// The rule: the exact line `thought` (blank space and lines before it
    /// allowed, nothing after it on the line) opens a model's reasoning
    /// sent as text; a start of that line is undecided; anything else,
    /// the word in a sentence or with other words on its line, is prose.
    #[test]
    fn the_thought_line_is_exact() {
        let cases = [
            ("thought\nPlan it.", Thought::Is("Plan it.")),
            ("\n  thought\r\n- a\n- b", Thought::Is("- a\n- b")),
            ("thought\n", Thought::Is("")),
            ("", Thought::Undecided),
            (" \n", Thought::Undecided),
            ("th", Thought::Undecided),
            ("thought", Thought::Undecided),
            ("thought\r", Thought::Undecided),
            ("thought about it", Thought::Not),
            ("thoughts\nx", Thought::Not),
            ("thought \nx", Thought::Not),
            ("Thought: x", Thought::Not),
            ("Thought\nx", Thought::Not),
            ("the plan", Thought::Not),
            ("I thought\nso", Thought::Not),
        ];
        for (text, expected) in cases {
            assert_eq!(leaked_thought(text), expected, "{text:?}");
        }
    }

    fn streamed(deltas: &[&str]) -> Transcript {
        let mut t = Transcript::new();
        t.apply_agent_event(&AgentEvent::TurnStarted {
            turn_number: 0,
            assistant_message_id: None,
        });
        for delta in deltas {
            t.apply_agent_event(&text(delta));
        }
        t
    }

    /// A text block opening with the line `thought` streams as a collapsed
    /// reasoning block without the line, however the deltas cut it (the
    /// word split, the line's end in the next delta); nothing shows while
    /// the line is undecided, and the rest of the step's text goes on in
    /// it. The answer after something visible is prose again.
    #[test]
    fn a_leaked_thought_streams_as_reasoning_in_any_cut() {
        let cuts: [&[&str]; 4] = [
            &["thought\nPlan: read.\n- a", "\n- b"],
            &["tho", "ught", "\nPlan: read.\n- a\n- b"],
            &["\n", "thought", "\n", "Plan: read.\n- a\n- b"],
            &["t", "hought\r\nPlan: read.\n- a\n- b"],
        ];
        for deltas in cuts {
            let mut held = Transcript::new();
            held.apply_agent_event(&AgentEvent::TurnStarted {
                turn_number: 0,
                assistant_message_id: None,
            });
            for (i, delta) in deltas.iter().enumerate() {
                held.apply_agent_event(&text(delta));
                if i + 1 < deltas.len() {
                    let shown: Vec<_> = held
                        .blocks()
                        .iter()
                        .flat_map(|b| block_lines(b, 60))
                        .map(|l| l.to_string())
                        .collect();
                    assert!(
                        !shown.iter().any(|l| l.starts_with('•')),
                        "{deltas:?}: {shown:?}"
                    );
                }
            }
            let reasoning = match held
                .blocks()
                .iter()
                .rev()
                .find(|b| !matches!(b, Block::Assistant { md } if md.trim().is_empty()))
            {
                Some(Block::Reasoning { text, collapsed }) => {
                    (text.trim_start().to_string(), *collapsed)
                }
                other => panic!("{deltas:?}: {other:?}"),
            };
            assert_eq!(
                reasoning,
                ("Plan: read.\n- a\n- b".to_string(), true),
                "{deltas:?}"
            );
        }
        // The answer in the next step is prose.
        let mut t = streamed(&["thought\nlook first"]);
        t.apply_agent_event(&AgentEvent::TurnStarted {
            turn_number: 1,
            assistant_message_id: None,
        });
        t.apply_agent_event(&text("The answer."));
        assert_eq!(
            t.blocks(),
            &[
                Block::Reasoning {
                    text: "look first".into(),
                    collapsed: true
                },
                Block::Assistant {
                    md: "The answer.".into()
                }
            ]
        );
        let lines: Vec<String> = block_lines(&t.blocks()[0], 60)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(lines[0], "✻ thinking (10 chars, Ctrl-O to expand)");
    }

    /// The word in a sentence, or a start of the line the step ends on,
    /// is prose as it was; a held start shows once it is decided.
    #[test]
    fn near_misses_of_the_thought_line_stay_prose() {
        for deltas in [
            &["thought", " about it"][..],
            &["Thought: x"],
            &["th", "e plan"],
            &["thought \nx"],
        ] {
            let t = streamed(deltas);
            assert_eq!(
                t.blocks(),
                &[Block::Assistant {
                    md: deltas.concat()
                }],
                "{deltas:?}"
            );
        }
        // Held to the step's end, it shows as it is.
        let mut t = streamed(&["thought"]);
        assert!(matches!(&t.blocks()[0], Block::Assistant { md } if md.is_empty()));
        t.apply_agent_event(&AgentEvent::TurnCompleted {
            stop_reason: StopReason::EndTurn,
            usage: test_usage(Usage::default()),
            assistant_message_id: None,
        });
        assert_eq!(
            t.blocks(),
            &[Block::Assistant {
                md: "thought".into()
            }]
        );
    }

    fn reasoning(d: &str) -> AgentEvent {
        AgentEvent::ReasoningDelta {
            delta: d.into(),
            assistant_message_id: None,
        }
    }

    fn reasoning_done(content: &str) -> AgentEvent {
        AgentEvent::ReasoningComplete {
            content: content.into(),
            assistant_message_id: None,
        }
    }

    fn text_done(content: &str) -> AgentEvent {
        AgentEvent::TextComplete {
            content: content.into(),
            assistant_message_id: None,
        }
    }

    fn turn_started() -> AgentEvent {
        AgentEvent::TurnStarted {
            turn_number: 0,
            assistant_message_id: None,
        }
    }

    /// A step's `TextComplete` is one assistant block: after text deltas,
    /// which accumulate into it, and with no deltas before it.
    #[test]
    fn text_complete_is_one_assistant_block_with_or_without_deltas() {
        for (deltas, whole) in [(&["Hel", "lo"][..], "Hello"), (&[], "whole")] {
            let mut t = Transcript::new();
            t.apply_agent_event(&turn_started());
            for delta in deltas {
                t.apply_agent_event(&text(delta));
            }
            t.apply_agent_event(&text_done(whole));
            assert_eq!(
                t.blocks(),
                &[Block::Assistant { md: whole.into() }],
                "{deltas:?}"
            );
        }
    }

    /// The event orders the engine's OpenAI dialect sends for one answer
    /// cut in a word (`litellm::an_answer_cut_across_parts_and_blank_reasoning_is_one_block`
    /// pins them on the wire): two text parts or message items (two text
    /// deltas), a blank or empty reasoning summary between (reasoning
    /// deltas, completed after the text), empty text deltas. Each is one
    /// prose block with the text joined as it came, and no reasoning block.
    #[test]
    fn one_step_s_text_is_one_block_across_blank_reasoning_and_empty_deltas() {
        let whole = "> the reader was removed; the new one";
        let orders: [&[AgentEvent]; 5] = [
            &[text("> the reader was remov"), text("ed; the new one")],
            &[
                text("> the reader was remov"),
                reasoning(""),
                text("ed; the new one"),
                reasoning_done(""),
            ],
            &[
                text("> the reader was remov"),
                reasoning(" "),
                reasoning("\n\n"),
                text("ed; the new one"),
                reasoning_done(" \n\n"),
            ],
            &[
                text(""),
                text("> the reader was remov"),
                text(""),
                text("ed; the new one"),
            ],
            &[
                reasoning_done("\n"),
                text("> the reader was remov"),
                text("ed; the new one"),
            ],
        ];
        for events in orders {
            let mut t = Transcript::new();
            t.apply_agent_event(&turn_started());
            for event in events {
                t.apply_agent_event(event);
            }
            t.apply_agent_event(&text_done(whole));
            assert_eq!(
                t.blocks(),
                &[Block::Assistant { md: whole.into() }],
                "{events:?}"
            );
        }
    }

    /// Something visible between two deltas parts the prose: a reasoning
    /// with words (it opens its block, and blank reasoning inside an open
    /// one is kept), a tool call, a notice. A `TextComplete` does not.
    #[test]
    fn only_something_visible_parts_the_prose() {
        let mut t = Transcript::new();
        t.apply_agent_event(&turn_started());
        t.apply_agent_event(&text("First."));
        t.apply_agent_event(&reasoning("weighing"));
        t.apply_agent_event(&reasoning("\n\n"));
        t.apply_agent_event(&reasoning("it"));
        t.apply_agent_event(&text("Second."));
        t.apply_agent_event(&text_done("First.Second."));
        t.apply_agent_event(&text(" still second"));
        assert_eq!(
            t.blocks(),
            &[
                Block::Assistant {
                    md: "First.".into()
                },
                Block::Reasoning {
                    text: "weighing\n\nit".into(),
                    collapsed: true,
                },
                Block::Assistant {
                    md: "Second. still second".into()
                },
            ]
        );
        t.push_notice(NoticeLevel::Warn, "retrying");
        t.apply_agent_event(&text("Third."));
        assert_eq!(t.blocks().len(), 5);
        // The next model step is a block of its own.
        t.apply_agent_event(&turn_started());
        t.apply_agent_event(&text("Fourth."));
        assert_eq!(t.blocks().len(), 6);
    }

    /// A helper's view takes the same path: its text cut by a blank
    /// reasoning is one prose block of its own, in its view.
    #[test]
    fn a_helper_s_text_is_one_block_across_blank_reasoning() {
        let mut t = Transcript::new();
        t.push(Block::User { text: "go".into() });
        t.apply_scoped_event(&helper_event(&["scout"], turn_started()));
        t.apply_scoped_event(&helper_event(&["scout"], text("found the remov")));
        t.apply_scoped_event(&helper_event(&["scout"], reasoning(" ")));
        t.apply_scoped_event(&helper_event(&["scout"], text("ed reader")));
        t.apply_scoped_event(&helper_event(&["scout"], reasoning_done(" ")));
        t.apply_scoped_event(&helper_event(
            &["scout"],
            text_done("found the removed reader"),
        ));
        let header = t.helper_header("scout").expect("helper header");
        let prose: Vec<&Block> = t
            .helper_view_blocks(header)
            .into_iter()
            .filter_map(|id| t.block(id))
            .filter(|b| matches!(b, Block::Assistant { .. } | Block::Reasoning { .. }))
            .collect();
        assert_eq!(
            prose,
            [&Block::Assistant {
                md: "found the removed reader".into()
            }]
        );
    }

    /// The event order of an owner's report (invented
    /// names): the agent's answer streams, a helper's tool line (its call,
    /// start and end) arrives in the middle of a Markdown link, then the
    /// rest of the answer and its `TextComplete`. The answer is one block,
    /// the helper's line after it; the agent's own tool call still parts
    /// its prose.
    #[test]
    fn a_helper_s_line_does_not_part_the_agent_s_streaming_prose() {
        let mut t = Transcript::new();
        t.push(Block::User { text: "go".into() });
        t.apply_agent_event(&turn_started());
        t.apply_agent_event(&text("Done.\n\nThe [pipeline](https://forge.example/gro"));
        t.apply_scoped_event(&helper_call(&["watcher"], "h1", "skill", json!({})));
        t.apply_scoped_event(&helper_event(
            &["watcher"],
            AgentEvent::ToolExecutionStarted {
                id: "h1".into(),
                name: "skill".into(),
            },
        ));
        t.apply_scoped_event(&helper_event(
            &["watcher"],
            AgentEvent::ToolExecutionCompleted {
                id: "h1".into(),
                name: "skill".into(),
                is_error: false,
                duration_ms: 10,
            },
        ));
        t.apply_agent_event(&text("up/1) is running."));
        t.apply_scoped_event(&helper_call(&["watcher"], "h2", "shell", json!({})));
        t.apply_agent_event(&text(" I watch it."));
        let whole =
            "Done.\n\nThe [pipeline](https://forge.example/group/1) is running. I watch it.";
        t.apply_agent_event(&text_done(whole));
        let shown: Vec<String> = t
            .blocks()
            .iter()
            .map(|b| match b {
                Block::User { .. } => "user".to_string(),
                Block::Assistant { md } => format!("prose {md}"),
                Block::Helper(fold) => format!("header {}", fold.label),
                Block::Tool {
                    tool,
                    tag: Some(tag),
                    ..
                } => format!("tool {tag} {tool}"),
                Block::Tool { tool, .. } => format!("tool {tool}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            shown,
            [
                "user".to_string(),
                format!("prose {whole}"),
                "header watcher".into(),
                "tool [watcher] skill".into(),
                "tool [watcher] shell".into(),
            ]
        );
        // The agent's own tool call parts it, as always.
        t.apply_agent_event(&AgentEvent::ToolCallRequested {
            id: "a1".into(),
            name: "read_file".into(),
            args: test_args(json!({"path": "x"})),
        });
        t.apply_agent_event(&text("After."));
        assert!(matches!(t.last(), Some(Block::Assistant { md }) if md == "After."));
    }

    /// None of a folded kept helper's events (its tool calls' start and
    /// end, its streamed text, its run's start) nor a peer message without
    /// words parts the agent's prose, and the blank message draws nothing.
    #[test]
    fn a_folded_helper_s_events_and_a_blank_peer_message_do_not_part_the_agent_s_prose() {
        let mut t = Transcript::new();
        t.push(Block::User { text: "go".into() });
        t.apply_agent_event(&turn_started());
        t.apply_scoped_event(&helper_event(&["scout"], turn_started()));
        t.apply_agent_event(&text("Then nothing will be"));
        t.apply_scoped_event(&helper_call(
            &["scout"],
            "h1",
            "shell",
            json!({"command": "ls"}),
        ));
        t.apply_scoped_event(&helper_event(
            &["scout"],
            AgentEvent::ToolExecutionStarted {
                id: "h1".into(),
                name: "shell".into(),
            },
        ));
        t.apply_scoped_event(&helper_event(&["scout"], text("its own words, folded")));
        t.peer_message(BlockId(0), "scout".into(), String::new());
        t.peer_message(BlockId(0), "scout".into(), "  \n".into());
        t.apply_agent_event(&text(" restored or discarded"));
        t.apply_scoped_event(&helper_event(
            &["scout"],
            AgentEvent::ToolExecutionCompleted {
                id: "h1".into(),
                name: "shell".into(),
                is_error: false,
                duration_ms: 5,
            },
        ));
        t.apply_agent_event(&text(" blindly."));
        let whole = "Then nothing will be restored or discarded blindly.";
        t.apply_agent_event(&text_done(whole));
        let agent_prose: Vec<&Block> = t
            .iter()
            .filter(|(id, b)| t.owner_of(*id).is_none() && matches!(b, Block::Assistant { .. }))
            .map(|(_, b)| b)
            .collect();
        assert_eq!(
            agent_prose,
            [&Block::Assistant { md: whole.into() }],
            "{:#?}",
            t.blocks()
        );
        assert!(
            !t.blocks().iter().any(|b| matches!(b, Block::Peer { .. })),
            "a blank peer message draws nothing"
        );
    }

    /// A kept helper's "went idle" note steered into the running turn is
    /// its folded line, never the user's `›` message.
    #[test]
    fn a_went_idle_note_joining_a_running_turn_is_its_folded_line() {
        use crate::daemon::agents::endings::{NoteItem, note_text};
        let mut t = Transcript::new();
        t.push(Block::User { text: "go".into() });
        t.apply_agent_event(&turn_started());
        let note = note_text(&[NoteItem {
            label: "scout".into(),
            how: crate::mobs::endings::How::WentIdle {
                answer: "found it".into(),
                failed: None,
            },
            rest: None,
            with: Vec::new(),
        }]);
        t.remote_input(note);
        assert!(
            matches!(t.last(), Some(Block::HelperNote { .. })),
            "{:?}",
            t.last()
        );
        assert_eq!(
            t.blocks()
                .iter()
                .filter(|b| matches!(b, Block::User { .. }))
                .count(),
            1
        );
    }

    /// The same rule the other way: the agent's tool call and notices of
    /// the session do not part a helper's streaming prose in its view.
    #[test]
    fn the_agent_s_blocks_do_not_part_a_helper_s_streaming_prose() {
        let mut t = Transcript::new();
        t.push(Block::User { text: "go".into() });
        t.apply_scoped_event(&helper_event(&["scout"], turn_started()));
        t.apply_scoped_event(&helper_event(&["scout"], text("see [the doc](https://ex")));
        t.apply_agent_event(&AgentEvent::ToolCallRequested {
            id: "a1".into(),
            name: "read_file".into(),
            args: test_args(json!({"path": "x"})),
        });
        t.push_notice(NoticeLevel::Info, "a notice of the session");
        t.apply_scoped_event(&helper_event(&["scout"], text("ample.org/a) first")));
        let header = t.helper_header("scout").expect("helper header");
        let prose: Vec<&Block> = t
            .helper_view_blocks(header)
            .into_iter()
            .filter_map(|id| t.block(id))
            .filter(|b| matches!(b, Block::Assistant { .. }))
            .collect();
        assert_eq!(
            prose,
            [&Block::Assistant {
                md: "see [the doc](https://example.org/a) first".into()
            }]
        );
        // Its own tool call parts it.
        t.apply_scoped_event(&helper_call(&["scout"], "h1", "grep", json!({})));
        t.apply_scoped_event(&helper_event(&["scout"], text("next")));
        assert_eq!(
            t.helper_view_blocks(header)
                .into_iter()
                .filter(|id| matches!(t.block(*id), Some(Block::Assistant { .. })))
                .count(),
            2
        );
    }

    #[test]
    fn tool_splits_assistant_blocks_and_tracks_lifecycle() {
        let mut t = Transcript::new();
        let now = Instant::now();
        t.apply_agent_event(&text("before"));
        t.apply_agent_event(&AgentEvent::ToolCallRequested {
            id: "t1".into(),
            name: "read_file".into(),
            args: test_args(json!({"path": "a.rs"})),
        });
        let id = t.tool_block("t1");
        assert_eq!(id, Some(BlockId(1)));
        t.apply_agent_event_at(
            &AgentEvent::ToolExecutionStarted {
                id: "t1".into(),
                name: "read_file".into(),
            },
            now,
        );
        assert!(matches!(
            t.block(BlockId(1)),
            Some(Block::Tool { state: ToolState::Running { since }, .. }) if *since == now
        ));
        let long = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        t.apply_agent_event(&AgentEvent::ToolExecutionCompleted {
            id: "t1".into(),
            name: "read_file".into(),
            is_error: false,
            duration_ms: 7,
        });
        t.apply_agent_event(&test_tool_result("t1", &long));
        match t.block(BlockId(1)) {
            Some(Block::Tool {
                state,
                result_preview: Some(preview),
                ..
            }) => {
                assert_eq!(
                    *state,
                    ToolState::Done {
                        is_error: false,
                        duration_ms: 7
                    }
                );
                assert_eq!(preview.lines().count(), MAX_TOOL_RESULT_LINES + 1);
                assert!(preview.ends_with("... (10 more lines)"));
            }
            other => panic!("unexpected block {other:?}"),
        }
        t.apply_agent_event(&text("after"));
        assert_eq!(t.len(), 3);
        assert_eq!(t.blocks()[2], Block::Assistant { md: "after".into() });
    }

    #[test]
    fn hook_denied_marks_tool_and_adds_approval_block() {
        let mut t = Transcript::new();
        t.apply_agent_event(&AgentEvent::ToolCallRequested {
            id: "t9".into(),
            name: "shell".into(),
            args: test_args(json!({"command": "rm -rf /"})),
        });
        t.apply_agent_event(&AgentEvent::HookDenied {
            hook_id: "tui-auto-mode".into(),
            point: HookPoint::PreToolExecution,
            reason_code: meerkat_core::HookReasonCode::PolicyViolation,
            message: "destructive".into(),
            payload: Some(json!({"tool_use_id": "t9", "tier": "classifier"})),
        });
        assert!(matches!(
            t.block(BlockId(0)),
            Some(Block::Tool { state: ToolState::Denied { reason }, .. }) if reason == "destructive"
        ));
        assert_eq!(
            t.blocks()[1],
            Block::Approval {
                tool: "shell".into(),
                tier: "classifier".into(),
                allowed: false,
                reason: Some("destructive".into()),
            }
        );
    }

    #[test]
    fn turn_completed_returns_usage_and_reasoning_is_collapsed() {
        let mut t = Transcript::new();
        t.apply_agent_event(&AgentEvent::ReasoningDelta {
            delta: "hmm".into(),
            assistant_message_id: None,
        });
        assert!(matches!(
            t.blocks()[0],
            Block::Reasoning {
                collapsed: true,
                ..
            }
        ));
        assert!(t.toggle_last_reasoning());
        assert!(matches!(
            t.blocks()[0],
            Block::Reasoning {
                collapsed: false,
                ..
            }
        ));
        let usage = t.apply_agent_event(&AgentEvent::TurnCompleted {
            stop_reason: StopReason::EndTurn,
            usage: test_usage(Usage {
                input_tokens: 5,
                output_tokens: 3,
                ..Usage::default()
            }),
            assistant_message_id: None,
        });
        assert_eq!(usage.map(|u| u.output_tokens), Some(3));
    }

    #[test]
    fn notices_for_retry_and_run_failed() {
        let mut t = Transcript::new();
        t.apply_agent_event(&AgentEvent::Retrying {
            retry: LlmRetrySchedule {
                failure: meerkat_core::LlmRetryFailure {
                    provider: "anthropic".into(),
                    kind: meerkat_core::LlmRetryFailureKind::NetworkTimeout,
                    retry_after_ms: None,
                    duration_ms: None,
                    message: "boom".into(),
                },
                plan: meerkat_core::LlmRetryPlan {
                    attempt: 1,
                    max_retries: 3,
                    computed_delay_ms: 10,
                    selected_delay_ms: 10,
                    retry_after_hint_ms: None,
                    rate_limit_floor_applied: false,
                    budget_capped: false,
                },
            },
            assistant_message_id: None,
        });
        t.apply_agent_event(&AgentEvent::RunFailed {
            session_id: meerkat_core::SessionId::new(),
            identity: meerkat_core::types::TranscriptMessageIdentity::default(),
            error_report: meerkat_core::AgentErrorReport {
                class: meerkat_core::AgentErrorClass::Llm,
                reason: None,
                message: "dead".into(),
            },
            terminal_cause_kind: None,
        });
        assert!(matches!(
            &t.blocks()[0],
            Block::Notice { level: NoticeLevel::Warn, text } if text.contains("retry 1/3")
        ));
        assert!(matches!(
            &t.blocks()[1],
            Block::Notice { level: NoticeLevel::Error, text } if text.contains("dead")
        ));
    }

    #[test]
    fn scoped_sub_agent_tools_are_nested() {
        let mut t = Transcript::new();
        let ev = ScopedAgentEvent::new(
            vec![
                StreamScopeFrame::Primary {
                    session_id: "s".into(),
                },
                StreamScopeFrame::MobMember {
                    flow_run_id: "f1".into(),
                    agent_identity: "helper".into(),
                    agent_runtime_id: None,
                    fence_token: None,
                    generation: None,
                },
            ],
            AgentEvent::ToolCallRequested {
                id: "x".into(),
                name: "glob".into(),
                args: test_args(json!({})),
            },
        );
        t.apply_scoped_event(&ev);
        // The helper's header, then its tool block.
        assert!(matches!(&t.blocks()[0], Block::Helper(fold) if fold.label == "helper"));
        assert!(matches!(
            &t.blocks()[1],
            Block::Tool { tool, tag: Some(tag), depth: 1, .. }
                if tool == "glob" && tag == "[helper]"
        ));
        // Primary-only scope is ignored.
        t.apply_scoped_event(&ScopedAgentEvent::primary("s", text("dup")));
        assert_eq!(t.len(), 2);
        // Once its model is known, a helper's blocks name it; a helper of a
        // helper nests one level deeper.
        t.note_helper_model("inner", "gpt-5.5");
        let mut nested = ev.clone();
        nested.scope_path.push(StreamScopeFrame::MobMember {
            flow_run_id: String::new(),
            agent_identity: "inner".into(),
            agent_runtime_id: None,
            fence_token: None,
            generation: None,
        });
        nested.scope_id = ScopedAgentEvent::scope_id_from_path(&nested.scope_path);
        t.apply_scoped_event(&nested);
        assert!(matches!(
            &t.blocks()[3],
            Block::Tool { tool, tag: Some(tag), depth: 2, .. }
                if tool == "glob" && tag == "[inner · gpt-5.5]"
        ));
    }

    /// `event` of the helper `path` (outermost first).
    fn helper_event(path: &[&str], event: AgentEvent) -> ScopedAgentEvent {
        let mut frames = vec![StreamScopeFrame::Primary {
            session_id: "s".into(),
        }];
        frames.extend(path.iter().map(|label| StreamScopeFrame::MobMember {
            flow_run_id: String::new(),
            agent_identity: (*label).into(),
            agent_runtime_id: None,
            fence_token: None,
            generation: None,
        }));
        ScopedAgentEvent::new(frames, event)
    }

    fn helper_call(path: &[&str], id: &str, name: &str, args: Value) -> ScopedAgentEvent {
        helper_event(
            path,
            AgentEvent::ToolCallRequested {
                id: id.into(),
                name: name.into(),
                args: test_args(args),
            },
        )
    }

    fn fold_of(t: &Transcript, label: &str) -> HelperFold {
        let id = t.helper_header(label).expect("helper header");
        match t.block(id) {
            Some(Block::Helper(fold)) => fold.clone(),
            other => panic!("not a helper header: {other:?}"),
        }
    }

    fn note_of(t: &Transcript, id: BlockId) -> Option<String> {
        match t.block(id) {
            Some(Block::Tool { note, .. }) => note.clone(),
            other => panic!("not a tool block: {other:?}"),
        }
    }

    #[test]
    fn notes_attach_to_tool_blocks_by_call_id() {
        let mut t = Transcript::new();
        t.apply_agent_event(&AgentEvent::ToolCallRequested {
            id: "t1".into(),
            name: "shell".into(),
            args: test_args(json!({"command": "rm -rf scratch.txt"})),
        });
        t.attach_note("t1", "allowed by classifier: in-project cleanup".into());
        assert_eq!(
            note_of(&t, BlockId(0)).as_deref(),
            Some("allowed by classifier: in-project cleanup")
        );
        // The note is one dim line under the header, shown while collapsed.
        let lines = block_lines(t.block(BlockId(0)).unwrap(), 80);
        let second: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(second, "  allowed by classifier: in-project cleanup");
        let dim = |style: ratatui::style::Style| {
            style.add_modifier.contains(ratatui::style::Modifier::DIM)
        };
        assert!(dim(lines[1].style) || lines[1].spans.iter().all(|s| dim(s.style)));
        // A note that arrives before its call waits for the block.
        t.attach_note("t2", "allowed by classifier: early".into());
        t.apply_agent_event(&AgentEvent::ToolCallRequested {
            id: "t2".into(),
            name: "shell".into(),
            args: test_args(json!({"command": "ls"})),
        });
        assert_eq!(
            note_of(&t, BlockId(1)).as_deref(),
            Some("allowed by classifier: early")
        );
        // A call without a note has none.
        t.apply_agent_event(&AgentEvent::ToolCallRequested {
            id: "t3".into(),
            name: "read_file".into(),
            args: test_args(json!({"path": "a.rs"})),
        });
        assert_eq!(note_of(&t, BlockId(2)), None);
    }

    #[test]
    fn a_helper_calls_note_hides_with_its_fold() {
        let mut t = Transcript::new();
        t.push(Block::User { text: "go".into() });
        t.apply_scoped_event(&helper_call(
            &["scout"],
            "h1",
            "shell",
            json!({"command": "make"}),
        ));
        let tool = BlockId(2);
        // The engine names the call by the id the helper's model issued.
        t.attach_note("h1", "allowed by classifier: builds the project".into());
        assert_eq!(
            note_of(&t, tool).as_deref(),
            Some("allowed by classifier: builds the project")
        );
        // Folded: the user block and the header only.
        assert_eq!(t.ensure_layout(80), 2 + 2);
        assert!(t.is_hidden(tool));
        let header = t.helper_header("scout").unwrap();
        assert!(t.set_helper_expanded(header, true));
        // Expanded: the tool block shows header, note and spacer.
        assert_eq!(t.ensure_layout(80), 2 + 2 + 3);
    }

    #[test]
    fn helper_events_fold_into_one_header() {
        let mut t = Transcript::new();
        t.note_helper_model("scout", "claude-opus-5-5");
        t.push(Block::User { text: "go".into() });
        t.apply_scoped_event(&helper_call(
            &["scout"],
            "a",
            "read_file",
            json!({"path": "a.rs"}),
        ));
        t.apply_scoped_event(&helper_call(
            &["scout"],
            "b",
            "grep",
            json!({"pattern": "fn resolve", "path": "src"}),
        ));
        t.apply_scoped_event(&helper_event(&["scout"], test_tool_result("b", "x\ny")));
        // The header first, then its two tool blocks.
        assert_eq!(t.len(), 4);
        let fold = fold_of(&t, "scout");
        assert_eq!(t.helper_header("scout"), Some(BlockId(1)));
        assert_eq!(fold.calls, 2);
        assert_eq!(fold.state, HelperState::Running);
        assert!(!fold.expanded);
        assert_eq!(fold.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(
            fold.last,
            HelperLast::Tool {
                name: "grep".into(),
                args: "\"fn resolve\"".into()
            }
        );
        assert_eq!(
            fold.status_text(),
            "⠋ running · 2 tool calls · grep \"fn resolve\""
        );
        // Folded: only the user block and the header lay out.
        let folded = t.ensure_layout(80);
        assert_eq!(folded, 2 + 2);
        assert!(t.is_hidden(BlockId(2)) && t.is_hidden(BlockId(3)));
        assert_eq!(t.last_tool_with_output(), None);
        // Text replaces the preview with its first line.
        t.apply_scoped_event(&helper_event(
            &["scout"],
            AgentEvent::TextComplete {
                content: "\nFound it in resolve.rs.\nMore.".into(),
                assistant_message_id: None,
            },
        ));
        assert_eq!(
            fold_of(&t, "scout").last,
            HelperLast::Text("Found it in resolve.rs.".into())
        );
    }

    #[test]
    fn helper_text_and_reasoning_become_blocks_owned_by_its_fold() {
        let mut t = Transcript::new();
        t.push(Block::User { text: "go".into() });
        let started = || AgentEvent::TurnStarted {
            turn_number: 0,
            assistant_message_id: None,
        };
        let reasoning = |d: &str| AgentEvent::ReasoningDelta {
            delta: d.into(),
            assistant_message_id: None,
        };
        let text = |d: &str| AgentEvent::TextDelta {
            delta: d.into(),
            assistant_message_id: None,
        };
        t.apply_scoped_event(&helper_event(&["scout"], started()));
        t.apply_scoped_event(&helper_event(&["scout"], reasoning("Look ")));
        t.apply_scoped_event(&helper_event(&["scout"], reasoning("around.")));
        t.apply_scoped_event(&helper_event(&["scout"], text("First ")));
        // The session's own text in between neither closes nor joins the
        // helper's.
        t.apply_agent_event(&text("main"));
        t.apply_scoped_event(&helper_event(&["scout"], text("finding.\nMore.")));
        t.apply_scoped_event(&helper_event(
            &["scout"],
            AgentEvent::TextComplete {
                content: "First finding.\nMore.".into(),
                assistant_message_id: None,
            },
        ));
        t.apply_scoped_event(&helper_call(&["scout"], "a", "glob", json!({})));
        // A completion without deltas makes its block.
        t.apply_scoped_event(&helper_event(&["scout"], started()));
        t.apply_scoped_event(&helper_event(
            &["scout"],
            AgentEvent::ReasoningComplete {
                content: "Done looking.".into(),
                assistant_message_id: None,
            },
        ));
        t.apply_scoped_event(&helper_event(
            &["scout"],
            AgentEvent::TextComplete {
                content: "Second.".into(),
                assistant_message_id: None,
            },
        ));
        let header = t.helper_header("scout").unwrap();
        let view: Vec<Block> = t
            .helper_view_blocks(header)
            .into_iter()
            .filter_map(|id| t.block(id).cloned())
            .collect();
        assert_eq!(view.len(), 5, "{view:?}");
        assert!(
            matches!(&view[0], Block::Reasoning { text, collapsed: true } if text == "Look around.")
        );
        assert!(matches!(&view[1], Block::Assistant { md } if md == "First finding.\nMore."));
        assert!(matches!(&view[2], Block::Tool { tool, tag: Some(tag), .. }
            if tool == "glob" && tag == "[scout]"));
        assert!(matches!(&view[3], Block::Reasoning { text, .. } if text == "Done looking."));
        assert!(matches!(&view[4], Block::Assistant { md } if md == "Second."));
        // The session's text is its own block, outside the helper.
        let main = t
            .blocks()
            .iter()
            .position(|b| matches!(b, Block::Assistant { md } if md == "main"))
            .unwrap();
        assert!(!t.helper_view_blocks(header).contains(&BlockId(main)));
        assert!(!t.is_hidden(BlockId(main)));
        // Folded, the helper's prose and reasoning are hidden; the preview
        // is the first line of its latest text.
        assert!(
            t.helper_view_blocks(header)
                .iter()
                .all(|id| t.is_hidden(*id))
        );
        assert_eq!(
            fold_of(&t, "scout").last,
            HelperLast::Text("Second.".into())
        );
        // Ctrl-O on the transcript skips the hidden helper reasoning; in
        // the view it toggles the helper's latest.
        assert!(!t.toggle_last_reasoning());
        assert!(t.toggle_last_reasoning_in(Some(header)));
        let latest = t.helper_view_blocks(header)[3];
        assert!(matches!(
            t.block(latest),
            Some(Block::Reasoning {
                collapsed: false,
                ..
            })
        ));
        // The view lays out every block, folded or not.
        let lines = t.helper_view_lines(header, 80);
        let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert!(
            text.iter().any(|l| l.contains("✻ thinking (12 chars")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|l| l.contains("First finding.")),
            "{text:?}"
        );
    }

    #[test]
    fn an_ended_helper_leaves_the_panel_after_its_linger_without_a_turn() {
        let now = Instant::now();
        let mut t = Transcript::new();
        t.seated_helpers(["a".to_string(), "b".to_string()]);
        t.note_live_helpers([("a", "m", 1), ("b", "m", 1)]);
        t.settle_helpers(now);
        let (a, b) = (t.helper_header("a").unwrap(), t.helper_header("b").unwrap());
        assert_eq!(t.panel_helpers(None), vec![b, a]);
        // a leaves its mob: still listed, after the live one, until its
        // linger is over.
        t.note_live_helpers([("b", "m", 1)]);
        t.seated_helpers(["b".to_string()]);
        t.settle_helpers(now);
        assert_eq!(t.panel_helpers(None), vec![b, a]);
        assert!(t.helpers_lingering());
        t.settle_helpers(now + PANEL_LINGER);
        assert_eq!(t.panel_helpers(None), vec![b]);
        assert!(!t.helpers_lingering());
        // Removing an earlier block leaves the panel's ids as they were.
        t.push(Block::User { text: "x".into() });
        assert!(t.remove_block(a).is_some());
        assert_eq!(t.panel_helpers(None), vec![b]);
    }

    #[test]
    fn expanding_a_helper_reveals_its_blocks() {
        let mut t = Transcript::new();
        t.apply_scoped_event(&helper_call(
            &["scout"],
            "a",
            "glob",
            json!({"pattern": "*"}),
        ));
        t.apply_scoped_event(&helper_event(&["scout"], test_tool_result("a", "one")));
        let header = t.helper_header("scout").unwrap();
        assert_eq!(t.ensure_layout(80), 2);
        assert_eq!(t.toggle_helper(header), Some(true));
        assert_eq!(t.ensure_layout(80), 4);
        assert!(!t.is_hidden(BlockId(1)));
        assert_eq!(t.last_tool_with_output(), Some(BlockId(1)));
        assert_eq!(t.toggle_helper(header), Some(false));
        assert_eq!(t.ensure_layout(80), 2);
        // Not a header.
        assert_eq!(t.toggle_helper(BlockId(1)), None);
        // The expanded setting starts new headers open.
        let mut t = Transcript::new();
        t.set_helper_view(HelperView::Expanded);
        t.apply_scoped_event(&helper_call(&["scout"], "a", "glob", json!({})));
        assert!(fold_of(&t, "scout").expanded);
        assert_eq!(t.ensure_layout(80), 4);
    }

    #[test]
    fn a_nested_helper_folds_inside_its_parent() {
        let mut t = Transcript::new();
        t.apply_scoped_event(&helper_call(&["outer"], "a", "glob", json!({})));
        t.apply_scoped_event(&helper_call(&["outer", "inner"], "b", "grep", json!({})));
        // outer header, its tool, inner header, inner's tool.
        assert_eq!(t.len(), 4);
        let outer = t.helper_header("outer").unwrap();
        let inner = t.helper_header("inner").unwrap();
        assert_eq!(fold_of(&t, "inner").depth, 2);
        assert!(t.is_hidden(inner));
        assert_eq!(t.ensure_layout(80), 2);
        // Opening the parent shows the nested helper folded.
        t.toggle_helper(outer);
        assert!(!t.is_hidden(inner));
        assert!(t.is_hidden(BlockId(3)));
        assert_eq!(t.ensure_layout(80), 6);
        assert_eq!(
            t.visible_helper_headers().collect::<Vec<_>>(),
            vec![outer, inner]
        );
        t.toggle_helper(inner);
        assert_eq!(t.ensure_layout(80), 8);
        // Folding the parent hides everything below it again.
        t.toggle_helper(outer);
        assert_eq!(t.ensure_layout(80), 2);
        assert_eq!(t.visible_helper_headers().collect::<Vec<_>>(), vec![outer]);
        // The parent's own call count is its own.
        assert_eq!(fold_of(&t, "outer").calls, 1);
        assert_eq!(fold_of(&t, "inner").calls, 1);
    }

    #[test]
    fn helper_finished_states_and_tokens() {
        let mut t = Transcript::new();
        t.note_live_helpers([("scout", "m", 1)]);
        // A newly live helper gets its header at once.
        assert_eq!(fold_of(&t, "scout").state, HelperState::Running);
        t.apply_scoped_event(&helper_call(&["scout"], "a", "glob", json!({})));
        t.apply_scoped_event(&helper_event(
            &["scout"],
            AgentEvent::TurnCompleted {
                stop_reason: StopReason::EndTurn,
                usage: test_usage(Usage {
                    input_tokens: 2000,
                    output_tokens: 100,
                    ..Usage::default()
                }),
                assistant_message_id: None,
            },
        ));
        t.apply_scoped_event(&helper_event(
            &["scout"],
            AgentEvent::RunCompleted {
                session_id: meerkat_core::SessionId::new(),
                identity: Default::default(),
                result: "ok".into(),
                structured_output: None,
                extraction_required: false,
                usage: Default::default(),
                terminal_cause_kind: None,
                assistant_message_id: None,
            },
        ));
        let fold = fold_of(&t, "scout");
        assert_eq!(fold.state, HelperState::Done);
        assert_eq!(fold.usage, Some(2100));
        assert_eq!(fold.status_text(), "done · 1 tool call · 2.1k tokens");
        // Leaving the live list keeps it done; a new spawn of the label gets
        // a fresh header.
        t.note_live_helpers(std::iter::empty());
        t.note_live_helpers([("scout", "m", 1)]);
        assert_eq!(t.helper_headers().count(), 2);
        assert_eq!(fold_of(&t, "scout").state, HelperState::Running);
        // A failure and a cancellation.
        let failed = |class| {
            helper_event(
                &["scout"],
                AgentEvent::RunFailed {
                    session_id: meerkat_core::SessionId::new(),
                    identity: Default::default(),
                    error_report: meerkat_core::AgentErrorReport {
                        class,
                        reason: None,
                        message: "dead".into(),
                    },
                    terminal_cause_kind: None,
                },
            )
        };
        t.apply_scoped_event(&failed(meerkat_core::AgentErrorClass::Llm));
        assert_eq!(fold_of(&t, "scout").state, HelperState::Failed);
        assert_eq!(fold_of(&t, "scout").status_text(), "failed · 0 tool calls");
        // The error notice belongs to the helper.
        let last = BlockId(t.len() - 1);
        assert!(matches!(t.block(last), Some(Block::Notice { .. })));
        assert!(t.is_hidden(last));
        t.apply_scoped_event(&failed(meerkat_core::AgentErrorClass::Cancelled));
        assert_eq!(fold_of(&t, "scout").state, HelperState::Cancelled);
        // A helper that left the live list while running is done.
        t.note_live_helpers([("other", "m", 1)]);
        t.note_live_helpers(std::iter::empty());
        assert_eq!(fold_of(&t, "other").state, HelperState::Done);
    }

    #[test]
    fn running_helpers_spin_and_history_helpers_start_folded_and_finished() {
        let mut t = Transcript::new();
        let start = Instant::now();
        t.apply_scoped_event(&helper_call(&["scout"], "a", "glob", json!({})));
        let later = start + std::time::Duration::from_secs(1);
        assert!(t.tick_helpers(later));
        assert_ne!(fold_of(&t, "scout").spin, 0);
        // A resumed transcript's helper headers are folded and finished.
        let mut fold = fold_of(&t, "scout");
        fold.expanded = true;
        t.reset(vec![Block::Helper(fold)]);
        let fold = fold_of(&t, "scout");
        assert!(!fold.expanded);
        assert_eq!(fold.state, HelperState::Done);
        assert!(t.has_helpers());
        assert!(!t.tick_helpers(later));
    }

    #[test]
    fn layout_cache_invalidates_on_mutation() {
        let mut t = Transcript::new();
        t.apply_agent_event(&text("one"));
        let h1 = t.ensure_layout(40);
        t.apply_agent_event(&text("\ntwo\nthree"));
        let h2 = t.ensure_layout(40);
        assert_eq!(h2, h1 + 2);
    }

    /// Every way a block changes reaches both of its layouts and the line
    /// counts from it on, and leaves the blocks before it alone.
    #[test]
    fn an_edit_invalidates_the_blocks_layouts_and_the_counts_after_it() {
        let mut t = Transcript::new();
        let a = t.push(Block::User { text: "a".into() });
        let b = t.push(Block::User { text: "b".into() });
        let c = t.push(Block::User { text: "c".into() });
        t.ensure_layout(40);
        for index in 0..3 {
            t.entries[index].view_layout = Some(CachedLayout {
                width: 40,
                lines: Vec::new(),
            });
        }
        assert!(t.start_line(c).is_some());
        assert!(t.block_mut(b).is_some());
        assert!(t.start_line(a).is_some(), "an earlier block stays laid out");
        assert!(t.start_line(c).is_none(), "a later block's start is stale");
        let entry = t.entry(b).expect("b");
        assert!(entry.layout.is_none() && entry.view_layout.is_none());
        assert!(t.entry(a).expect("a").view_layout.is_some());
        t.ensure_layout(40);
        assert!(t.start_line(c).is_some());
        t.invalidate_all();
        assert!(t.start_line(b).is_none());
        assert!(
            t.entries
                .iter()
                .all(|e| e.layout.is_none() && e.view_layout.is_none())
        );
    }

    #[test]
    fn replace_and_remove_blocks_keep_order_and_ids() {
        let mut t = Transcript::new();
        t.push(Block::User { text: "a".into() });
        let q = t.push(Block::Queued {
            id: PendingId(1),
            text: "b".into(),
            label: QueuedLabel::Queued,
        });
        t.apply_agent_event(&AgentEvent::ToolCallRequested {
            id: "t1".into(),
            name: "glob".into(),
            args: test_args(json!({})),
        });
        assert_eq!(t.tool_block("t1"), Some(BlockId(2)));
        assert!(t.replace_block(q, Block::User { text: "b".into() }));
        assert_eq!(t.block(q), Some(&Block::User { text: "b".into() }));
        assert!(!t.replace_block(BlockId(9), Block::User { text: "x".into() }));
        assert_eq!(
            t.remove_block(BlockId(1)),
            Some(Block::User { text: "b".into() })
        );
        assert_eq!(t.len(), 2);
        // The ids after it stay: an id names its block for good.
        assert_eq!(t.tool_block("t1"), Some(BlockId(2)));
        assert!(matches!(t.block(BlockId(2)), Some(Block::Tool { .. })));
        assert!(t.block(BlockId(1)).is_none());
        assert!(t.remove_block(BlockId(5)).is_none());
        assert_eq!(t.ensure_layout(40), t.visible_lines(0, 100).len());
    }

    /// A long transcript lays out only what changed, and a frame touches
    /// the blocks it shows, not the whole transcript: one streamed delta
    /// into the last of 10 000 blocks lays out that block alone, and
    /// drawing the last 40 lines visits a handful of blocks.
    #[test]
    fn a_large_transcript_lays_out_what_changed_and_walks_the_viewport() {
        let mut t = Transcript::new();
        for i in 0..10_000 {
            t.push(Block::User {
                text: format!("message {i}"),
            });
        }
        let total = t.ensure_layout(80);
        assert_eq!(t.layout.laid_out, 10_000);
        assert_eq!(total, t.line_count());
        let last = BlockId(t.len() - 1);
        if let Some(Block::User { text }) = t.block_mut(last) {
            text.push_str("\nmore");
        }
        let laid = t.layout.laid_out;
        let grown = t.ensure_layout(80);
        assert_eq!(t.layout.laid_out, laid + 1, "only the changed block");
        assert_eq!(grown, total + 1);
        t.layout.walked.set(0);
        let lines = t.visible_lines(grown - 40, 40);
        let blocks = t.visible_blocks(grown - 40, 40);
        assert_eq!(lines.len(), 40);
        assert_eq!(blocks.len(), 40);
        assert_eq!(blocks.last(), Some(&last));
        assert!(
            t.layout.walked.get() < 100,
            "walked {} blocks",
            t.layout.walked.get()
        );
        // The lines are the same as a walk over everything gives.
        let all: Vec<String> = (0..grown)
            .map(|i| t.line(i).map(ToString::to_string).unwrap_or_default())
            .collect();
        let shown: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(shown, all[grown - 40..]);
        // A change in the middle re-sums from there; a new width lays out
        // everything again.
        if let Some(Block::User { text }) = t.block_mut(BlockId(5_000)) {
            text.push_str("\nmid");
        }
        let laid = t.layout.laid_out;
        assert_eq!(t.ensure_layout(80), grown + 1);
        assert_eq!(t.layout.laid_out, laid + 1);
        assert_eq!(t.visible_lines(0, grown + 1).len(), grown + 1);
        t.ensure_layout(60);
        assert_eq!(t.layout.laid_out, laid + 1 + 10_000);
    }

    #[test]
    fn visible_blocks_match_visible_lines() {
        let mut t = Transcript::new();
        t.push(Block::User { text: "a".into() });
        t.push(Block::User {
            text: "b\nc".into(),
        });
        let total = t.ensure_layout(40);
        assert_eq!(total, 5);
        assert_eq!(
            t.visible_blocks(0, 10),
            [0, 0, 1, 1, 1].map(BlockId).to_vec()
        );
        assert_eq!(t.visible_blocks(1, 2), [BlockId(0), BlockId(1)]);
        assert_eq!(t.visible_blocks(0, 10).len(), t.visible_lines(0, 10).len());
    }

    #[test]
    fn tool_output_starts_collapsed_and_toggles() {
        let mut t = Transcript::new();
        t.apply_agent_event(&AgentEvent::ToolCallRequested {
            id: "t1".into(),
            name: "glob".into(),
            args: test_args(json!({})),
        });
        let id = t.tool_block("t1").unwrap();
        // Nothing to show yet: not toggleable.
        assert_eq!(t.toggle_tool(id), None);
        assert_eq!(t.last_tool_with_output(), None);
        t.apply_agent_event(&AgentEvent::ToolExecutionCompleted {
            id: "t1".into(),
            name: "glob".into(),
            is_error: false,
            duration_ms: 1,
        });
        t.apply_agent_event(&test_tool_result("t1", "a\nb"));
        assert!(matches!(
            t.block(id),
            Some(Block::Tool {
                collapsed: true,
                ..
            })
        ));
        assert_eq!(t.ensure_layout(40), 2);
        assert_eq!(t.last_tool_with_output(), Some(id));
        assert_eq!(t.toggle_tool(id), Some(false));
        assert_eq!(t.ensure_layout(40), 4);
        assert_eq!(t.toggle_tool(id), Some(true));
        t.push(Block::User { text: "x".into() });
        assert_eq!(t.toggle_tool(BlockId(1)), None);
    }

    /// A preview keeps the whole output's line count: a capped one names
    /// the lines it cut, and an output whose last line looks like that
    /// trailer is counted as it is, not read back out of its text.
    #[test]
    fn a_preview_counts_the_whole_output() {
        assert_eq!(Preview::of("one").line_count(), 1);
        assert_eq!(Preview::of("a\nb").line_count(), 2);
        let long = (0..57)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let capped = Preview::of(&long);
        assert_eq!(capped.line_count(), 57);
        assert!(capped.ends_with("... (37 more lines)"), "{capped}");
        let exact = (0..20)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(Preview::of(&exact).line_count(), 20);
        // A tool whose own output ends like the trailer.
        let tricky = (0..20)
            .map(|i| i.to_string())
            .chain(std::iter::once("... (500 more lines)".to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(Preview::of(&tricky).line_count(), 21);
    }

    #[test]
    fn a_requested_compaction_with_nothing_to_fold_says_so() {
        let mut t = Transcript::new();
        t.apply_agent_event(&AgentEvent::RequestedCompactionSkipped {
            run_id: meerkat_core::lifecycle::RunId::new(),
            reason: meerkat_core::RequestedCompactionSkipReason::NothingToFold,
        });
        assert!(t.blocks().iter().any(|b| matches!(
            b,
            Block::Notice { level: NoticeLevel::Info, text }
                if text.starts_with("/compact: nothing was compacted")
        )));
    }

    /// A `send_message` block names its recipient first (by the name a
    /// `contacts` result gave it), folds a successful receipt to `sent to
    /// <name>` with the raw JSON behind the toggle, and shows a failed
    /// send's error unfolded.
    #[test]
    fn a_send_message_reads_recipient_first_and_folds_its_receipt() {
        let mut t = Transcript::new();
        let call = |t: &mut Transcript, id: &str, name: &str, args: Value| {
            t.apply_agent_event(&AgentEvent::ToolCallRequested {
                id: id.into(),
                name: name.into(),
                args: test_args(args),
            });
        };
        let done = |t: &mut Transcript, id: &str, name: &str, is_error: bool, result: &str| {
            t.apply_agent_event(&AgentEvent::ToolExecutionCompleted {
                id: id.into(),
                name: name.into(),
                is_error,
                duration_ms: 3,
            });
            t.apply_agent_event(&test_tool_result(id, result));
        };
        call(&mut t, "p1", "contacts", json!({}));
        done(
            &mut t,
            "p1",
            "contacts",
            false,
            r#"{"you":"main-aaaa (an agent of this workspace)","agents_of_this_workspace":[{"name":"main-bbbb","what":"working","peer_id":"ed25519:abc"}]}"#,
        );
        call(
            &mut t,
            "m1",
            "send_message",
            json!({"body": "I am changing src/lib.rs now", "handling_mode": "queue", "peer_id": "ed25519:abc"}),
        );
        done(
            &mut t,
            "m1",
            "send_message",
            false,
            r#"{"kind":"peer_message","receipt":{"delivery":{"durably_resolved":{"outcome":"accepted"}},"envelope_id":"e1","kind":"peer_message_sent"},"status":"sent"}"#,
        );
        let id = t.tool_block("m1").unwrap();
        let Some(Block::Tool {
            head,
            note,
            collapsed,
            result_preview,
            ..
        }) = t.block(id)
        else {
            panic!("no tool block");
        };
        assert_eq!(
            head.text(),
            "message → main-bbbb: I am changing src/lib.rs now"
        );
        assert_eq!(note.as_deref(), Some("sent to main-bbbb"));
        assert!(*collapsed);
        assert!(
            result_preview
                .as_ref()
                .is_some_and(|p| p.contains("peer_message_sent"))
        );
        // Unknown peer: its id; a failed send shows its error.
        call(
            &mut t,
            "m2",
            "send_message",
            json!({"body": "hi", "handling_mode": "queue", "peer_id": "ed25519:zzz"}),
        );
        done(
            &mut t,
            "m2",
            "send_message",
            true,
            "peer not found: ed25519:zzz",
        );
        let id = t.tool_block("m2").unwrap();
        let Some(Block::Tool {
            head,
            note,
            collapsed,
            ..
        }) = t.block(id)
        else {
            panic!("no tool block");
        };
        assert_eq!(head.text(), "message → ed25519:zzz: hi");
        assert!(note.is_none() && !*collapsed);
        // A `contacts` result longer than the preview (pretty, many
        // agents) still names its peers: the whole result is read, never
        // the cut preview.
        let many: Vec<Value> = (0..30)
            .map(|n| json!({"name": format!("main-{n:04}"), "what": "idle", "peer_id": format!("ed25519:p{n}")}))
            .collect();
        let long =
            serde_json::to_string_pretty(&json!({ "agents_of_this_workspace": many })).unwrap();
        call(&mut t, "p2", "contacts", json!({}));
        done(&mut t, "p2", "contacts", false, &long);
        call(
            &mut t,
            "m3",
            "send_message",
            json!({"body": "hi", "handling_mode": "queue", "peer_id": "ed25519:p29"}),
        );
        let id = t.tool_block("m3").unwrap();
        let Some(Block::Tool { head, .. }) = t.block(id) else {
            panic!("no tool block");
        };
        assert_eq!(head.text(), "message → main-0029: hi");
        // Other tools have their own lines, never JSON.
        assert_eq!(
            tool_line::head(
                "read_file",
                None,
                &json!({"path": "a"}),
                &tool_line::PathView::default()
            )
            .text(),
            "read a"
        );
    }
}
