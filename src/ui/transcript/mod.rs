//! Transcript model: the list of rendered blocks and the agent-event → block mapping.
//!
//! The mapping mirrors `meerkat-cli/src/stream_renderer.rs` but produces structured
//! blocks instead of ANSI text. Each block has a per-width layout cache that is
//! invalidated whenever the block mutates.

use std::time::Instant;

use indexmap::IndexMap;
use meerkat_core::types::text_content;
use meerkat_core::{AgentEvent, HookPoint, TurnUsage};
use ratatui::text::Line;
use serde_json::Value;

use super::delivery::PendingId;
use super::markdown::truncate_chars;
use super::render::{SPINNER, block_lines};

mod helpers;
mod notices;
mod restore;

use helpers::helper_args_preview;
pub use helpers::{HelperFold, HelperLast, HelperState, HelperView, MAX_HELPER_PREVIEW};
use notices::{compaction_skipped, retry_notice, upstream_notice};
pub use restore::blocks_from_messages;

/// Maximum tool-result lines kept in the preview (matches the CLI stream renderer).
pub const MAX_TOOL_RESULT_LINES: usize = 20;
/// Maximum characters of serialized tool args shown in a header (matches the CLI).
pub const MAX_TOOL_ARGS_PREVIEW: usize = 200;

/// Most notes kept for tool calls whose block has not arrived yet.
const MAX_PENDING_NOTES: usize = 64;

/// Index of a block in the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(pub usize);

/// Severity of a notice block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    /// Informational chrome.
    Info,
    /// Something degraded but the run continues.
    Warn,
    /// A failure.
    Error,
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
        /// How.
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
}

impl QueuedLabel {
    /// Text shown after the message.
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Delivering => "delivering…",
            Self::Deferred => "will send after this turn",
            Self::Shell => "runs after this turn",
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
    /// Whether the helper panel lists it: running, or finished during the
    /// current turn.
    #[must_use]
    pub fn in_panel(&self) -> bool {
        self.state.is_running() || self.this_turn
    }

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
        use std::fmt::Write as _;
        let mut out = String::new();
        if self.state.is_running() {
            let glyph = SPINNER
                .get(self.spin % SPINNER.len())
                .copied()
                .unwrap_or('⠋');
            out.push(glyph);
            out.push(' ');
        }
        out.push_str(self.state.text());
        let plural = if self.calls == 1 { "" } else { "s" };
        let _ = write!(out, " · {} tool call{plural}", self.calls);
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

/// A file diff attached to a tool block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffView {
    /// File the diff applies to.
    pub path: String,
    /// Unified diff text.
    pub unified: String,
    /// Added line count.
    pub added: usize,
    /// Removed line count.
    pub removed: usize,
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
        /// Message text.
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
        /// Reasoning text.
        text: String,
        /// Whether only a one-line summary is shown.
        collapsed: bool,
    },
    /// A tool call and its lifecycle.
    Tool {
        /// Tool name (sub-agent calls carry a scope label prefix).
        name: String,
        /// Arguments as requested by the model.
        args: Value,
        /// Lifecycle state.
        state: ToolState,
        /// Truncated result preview.
        result_preview: Option<String>,
        /// Attached file diff, if the tool edited a file.
        diff: Option<DiffView>,
        /// One dim line under the header, shown even when collapsed (e.g.
        /// `allowed by classifier: <reason>`).
        note: Option<String>,
        /// Nesting depth (0 for the primary agent, >0 for sub-agents).
        depth: usize,
        /// Whether the result preview and diff are hidden behind the header.
        /// Tool output starts collapsed; a click (or Ctrl-O for the latest) toggles it.
        collapsed: bool,
    },
    /// An approval decision.
    Approval {
        /// Tool the decision applies to.
        tool: String,
        /// Tier / mechanism that decided.
        tier: String,
        /// Whether the call was allowed.
        allowed: bool,
        /// Optional explanation.
        reason: Option<String>,
    },
    /// Chrome line (retry, budget, compaction, errors, command output).
    Notice {
        /// Severity.
        level: NoticeLevel,
        /// Text.
        text: String,
    },
    /// A helper's header (see [`HelperFold`]).
    Helper(HelperFold),
    /// A message another agent of the workspace sent this one (a peer
    /// message), labelled with its sender.
    Peer {
        /// The sending agent's name.
        from: String,
        /// The message (empty: delivered into a running turn, whose text
        /// the live events do not carry).
        text: String,
    },
    /// A run nobody typed: a schedule fired it, or a file watch
    /// (`scheduled: <name>`, `files changed: …`), with its input.
    Triggered {
        /// What fired it.
        label: String,
        /// The run's input.
        text: String,
    },
    /// A command the user ran with `!`, and its output.
    Shell {
        /// The command line.
        command: String,
        /// Lifecycle state.
        state: ShellState,
        /// The output preview (capped like a tool result).
        output: Option<String>,
        /// Whether the output is hidden behind the header.
        collapsed: bool,
    },
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
    blocks: Vec<Block>,
    by_tool_id: IndexMap<String, BlockId>,
    layout: Vec<Option<CachedLayout>>,
    /// The open prose and reasoning blocks deltas append to: the session's
    /// own under `None`, each helper's under its header.
    streams: IndexMap<Option<BlockId>, Stream>,
    /// The model of each helper (by label) the UI has heard of, for block
    /// labels.
    helper_models: IndexMap<String, String>,
    /// Per block, the header of the helper it belongs to (`None`: the
    /// session's own blocks and top-level helper headers).
    owner: Vec<Option<BlockId>>,
    /// The current header of each helper, by label.
    helper_folds: IndexMap<String, BlockId>,
    /// Helpers listed as running by the last live update.
    live_helpers: Vec<String>,
    /// How new helper headers start.
    helper_view: HelperView,
    /// Notes for tool calls whose block has not arrived yet, by tool-use id
    /// (at most [`MAX_PENDING_NOTES`], oldest dropped first).
    pending_notes: IndexMap<String, String>,
    /// The spinner clock of running helper headers.
    helper_clock: Option<Instant>,
    /// Number of helper headers.
    helper_count: usize,
    /// Per block, its layout at one width as a helper's full view draws it
    /// (folded helpers do not hide it there).
    view_layout: Vec<Option<CachedLayout>>,
    /// `prefix[i]`: the laid-out lines of blocks `[0, i)`, valid for
    /// `i <= clean` ([`Self::ensure_layout`]): a frame finds its first
    /// visible block by binary search instead of walking the transcript.
    prefix: Vec<usize>,
    /// Blocks `[0, clean)` have a layout at `laid_width` that `prefix`
    /// sums; any change to a block lowers it to that block
    /// ([`Self::touch`]).
    clean: usize,
    /// The width the clean blocks were laid out at.
    laid_width: u16,
    /// Blocks laid out so far (tests).
    #[cfg(test)]
    laid_out: usize,
    /// Blocks visited to find and copy visible lines (tests).
    #[cfg(test)]
    walked: std::cell::Cell<usize>,
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
}

impl Transcript {
    /// Create an empty transcript.
    pub fn new() -> Self {
        Self::default()
    }

    /// The blocks a toggle key looks at: the transcript's blocks on screen
    /// (`scope == None`) or the full view of the helper `scope`, newest
    /// first.
    fn scoped_blocks_rev(&self, scope: Option<BlockId>) -> Vec<BlockId> {
        match scope {
            Some(header) => {
                let mut ids = self.helper_view_blocks(header);
                ids.reverse();
                ids
            }
            None => (0..self.blocks.len())
                .rev()
                .map(BlockId)
                .filter(|id| !self.is_hidden(*id))
                .collect(),
        }
    }

    /// The transcript as plain text for `/export`: one `## ` header per
    /// shown block (user, assistant, reasoning, tool, approval, notice,
    /// helper, queued) and its text. Folded reasoning, collapsed tool output
    /// and the blocks of folded helpers keep only their headers, as on
    /// screen.
    #[must_use]
    pub fn export_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for (idx, block) in self.blocks.iter().enumerate() {
            if self.is_hidden(BlockId(idx)) {
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
                    name,
                    args,
                    state,
                    result_preview,
                    diff,
                    note,
                    collapsed,
                    ..
                } => {
                    let state = match state {
                        ToolState::Requested => "requested".to_string(),
                        ToolState::Running { .. } => "running".to_string(),
                        ToolState::Done {
                            is_error,
                            duration_ms,
                        } => format!(
                            "{} in {duration_ms} ms",
                            if *is_error { "failed" } else { "done" }
                        ),
                        ToolState::Denied { reason } => format!("denied: {reason}"),
                    };
                    let _ = writeln!(out, "## tool {name} {} ({state})", args_preview(args));
                    if let Some(note) = note {
                        let _ = writeln!(out, "{note}");
                    }
                    if let Some(diff) = diff {
                        let _ = writeln!(
                            out,
                            "edited {} (+{} -{})",
                            diff.path, diff.added, diff.removed
                        );
                    }
                    if !collapsed {
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
                Block::Peer { from, text } => {
                    let _ = writeln!(out, "## message from agent {from}");
                    body(&mut out, text);
                }
                Block::Triggered { label, text } => {
                    let _ = writeln!(out, "## {label}");
                    body(&mut out, text);
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
                Block::Helper(fold) => {
                    let _ = writeln!(out, "## helper {} {}", fold.tag(), fold.state.text());
                }
            }
        }
        out
    }

    /// Whether `block` is inside a folded helper (the renderer skips it).
    pub fn is_hidden(&self, block: BlockId) -> bool {
        let mut at = block;
        while let Some(Some(up)) = self.owner.get(at.0) {
            if matches!(self.block(*up), Some(Block::Helper(f)) if !f.expanded) {
                return true;
            }
            at = *up;
        }
        false
    }

    /// All blocks in order.
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Number of blocks.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether the transcript has no blocks.
    #[cfg(any(test, feature = "test-support"))]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Block by id.
    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.blocks.get(id.0)
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

    /// Append a notice block.
    pub fn push_notice(&mut self, level: NoticeLevel, text: impl Into<String>) -> BlockId {
        self.push(Block::Notice {
            level,
            text: text.into(),
        })
    }

    /// Remove every block (the helper view setting stays).
    pub fn clear(&mut self) {
        let helper_view = self.helper_view;
        *self = Self::default();
        self.helper_view = helper_view;
    }

    /// Replace the whole transcript with `blocks` (e.g. after resuming a
    /// session). Helper headers from history start folded and finished.
    pub fn reset(&mut self, blocks: Vec<Block>) {
        self.clear();
        for mut block in blocks {
            if let Block::Helper(fold) = &mut block {
                fold.expanded = false;
                fold.this_turn = false;
                if fold.state.is_running() {
                    fold.state = HelperState::Done;
                }
            }
            self.push_raw(block);
        }
    }

    /// Block `index` (or one before it) changed: the clean prefix ends
    /// there.
    pub(super) fn touch(&mut self, index: usize) {
        self.clean = self.clean.min(index);
    }

    /// Mutable access to a block; invalidates its layout.
    pub fn block_mut(&mut self, id: BlockId) -> Option<&mut Block> {
        self.touch(id.0);
        if let Some(slot) = self.layout.get_mut(id.0) {
            *slot = None;
        }
        if let Some(slot) = self.view_layout.get_mut(id.0) {
            *slot = None;
        }
        self.blocks.get_mut(id.0)
    }

    /// A run nobody typed was fired by `label` with input `prompt`: the
    /// prompt's block among the run's (from `run_first`) is relabelled,
    /// else the labelled block is appended.
    pub fn label_triggered(&mut self, run_first: usize, label: String, prompt: String) {
        let at = (run_first.min(self.blocks.len())..self.blocks.len())
            .rev()
            .find(|&i| matches!(&self.blocks[i], Block::User { text } if *text == prompt));
        let block = Block::Triggered {
            label,
            text: prompt,
        };
        match at {
            Some(i) => {
                self.replace_block(BlockId(i), block);
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

    /// Remove a block. Later block ids shift down by one; the tool-id index and
    /// open streams are fixed up, callers holding other ids must adjust them.
    pub fn remove_block(&mut self, id: BlockId) -> Option<Block> {
        if id.0 >= self.blocks.len() {
            return None;
        }
        let block = self.blocks.remove(id.0);
        self.touch(id.0);
        if matches!(block, Block::Helper(_)) {
            self.helper_count = self.helper_count.saturating_sub(1);
        }
        if id.0 < self.layout.len() {
            self.layout.remove(id.0);
        }
        if id.0 < self.view_layout.len() {
            self.view_layout.remove(id.0);
        }
        if id.0 < self.owner.len() {
            self.owner.remove(id.0);
        }
        let shift = |b: BlockId| if b.0 > id.0 { BlockId(b.0 - 1) } else { b };
        self.by_tool_id.retain(|_, b| *b != id);
        for b in self.by_tool_id.values_mut() {
            *b = shift(*b);
        }
        self.helper_folds.retain(|_, b| *b != id);
        for b in self.helper_folds.values_mut() {
            *b = shift(*b);
        }
        for slot in &mut self.owner {
            *slot = slot.filter(|b| *b != id).map(shift);
        }
        let streams = std::mem::take(&mut self.streams);
        self.streams = streams
            .into_iter()
            .filter(|(header, _)| *header != Some(id))
            .map(|(header, s)| {
                let open = |b: Option<BlockId>| b.filter(|b| *b != id).map(shift);
                (
                    header.map(shift),
                    Stream {
                        assistant: open(s.assistant),
                        reasoning: open(s.reasoning),
                        ..s
                    },
                )
            })
            .collect();
        Some(block)
    }

    /// Drop every cached layout (e.g. on resize).
    pub fn invalidate_all(&mut self) {
        self.clean = 0;
        for slot in self.layout.iter_mut().chain(self.view_layout.iter_mut()) {
            *slot = None;
        }
    }

    /// Toggle collapse on the most recent reasoning block on screen.
    /// Returns whether one existed.
    #[cfg(any(test, feature = "test-support"))]
    pub fn toggle_last_reasoning(&mut self) -> bool {
        self.toggle_last_reasoning_in(None)
    }

    /// Toggle collapse on the most recent reasoning block of the transcript
    /// on screen (`None`) or of the full view of the helper `scope`.
    /// Returns whether one existed.
    #[cfg(any(test, feature = "test-support"))]
    pub fn toggle_last_reasoning_in(&mut self, scope: Option<BlockId>) -> bool {
        let Some(id) = self
            .scoped_blocks_rev(scope)
            .into_iter()
            .find(|id| matches!(self.block(*id), Some(Block::Reasoning { .. })))
        else {
            return false;
        };
        if let Some(Block::Reasoning { collapsed, .. }) = self.block_mut(id) {
            *collapsed = !*collapsed;
        }
        true
    }

    /// Ctrl-O: show or hide the detail of the latest work in the transcript
    /// on screen (`None`) or in the full view of the helper `scope`: the
    /// most recent reasoning block and the most recent tool output together.
    /// Both expand when either is collapsed, else both collapse. Returns
    /// whether there was anything to toggle.
    pub fn toggle_last_detail_in(&mut self, scope: Option<BlockId>) -> bool {
        let blocks = self.scoped_blocks_rev(scope);
        let reasoning = blocks
            .iter()
            .copied()
            .find(|id| matches!(self.block(*id), Some(Block::Reasoning { .. })));
        let tool = blocks
            .iter()
            .copied()
            .find(|id| self.block(*id).is_some_and(tool_has_output));
        let targets: Vec<BlockId> = reasoning.into_iter().chain(tool).collect();
        if targets.is_empty() {
            return false;
        }
        let expand = targets.iter().any(|id| {
            matches!(
                self.block(*id),
                Some(
                    Block::Reasoning {
                        collapsed: true,
                        ..
                    } | Block::Tool {
                        collapsed: true,
                        ..
                    } | Block::Shell {
                        collapsed: true,
                        ..
                    }
                )
            )
        });
        for id in targets {
            if let Some(
                Block::Reasoning { collapsed, .. }
                | Block::Tool { collapsed, .. }
                | Block::Shell { collapsed, .. },
            ) = self.block_mut(id)
            {
                *collapsed = !expand;
            }
        }
        true
    }

    /// Expand or collapse the output of the tool block `id`. Returns the new
    /// `collapsed` state, or `None` when `id` is not a tool block with output.
    pub fn toggle_tool(&mut self, id: BlockId) -> Option<bool> {
        if !self.block(id).is_some_and(tool_has_output) {
            return None;
        }
        match self.block_mut(id) {
            Some(Block::Tool { collapsed, .. } | Block::Shell { collapsed, .. }) => {
                *collapsed = !*collapsed;
                Some(*collapsed)
            }
            _ => None,
        }
    }

    /// The most recent tool block on screen with output to show or hide.
    #[cfg(any(test, feature = "test-support"))]
    pub fn last_tool_with_output(&self) -> Option<BlockId> {
        self.last_tool_with_output_in(None)
    }

    /// The most recent tool block with output of the transcript on screen
    /// (`None`) or of the full view of the helper `scope`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn last_tool_with_output_in(&self, scope: Option<BlockId>) -> Option<BlockId> {
        self.scoped_blocks_rev(scope)
            .into_iter()
            .find(|id| self.block(*id).is_some_and(tool_has_output))
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
        if width != self.laid_width {
            self.laid_width = width;
            self.clean = 0;
        }
        let start = self.clean.min(self.blocks.len());
        self.prefix.truncate(start + 1);
        if self.prefix.is_empty() {
            self.prefix.push(0);
        }
        for i in start..self.blocks.len() {
            let stale = self
                .layout
                .get(i)
                .is_some_and(|slot| slot.as_ref().is_none_or(|c| c.width != width));
            if stale {
                let lines = match self.blocks.get(i) {
                    Some(_) if self.is_hidden(BlockId(i)) => Vec::new(),
                    Some(block) => block_lines(block, width),
                    None => Vec::new(),
                };
                #[cfg(test)]
                {
                    self.laid_out += 1;
                }
                if let Some(slot) = self.layout.get_mut(i) {
                    *slot = Some(CachedLayout { width, lines });
                }
            }
            let base = self.prefix.get(i).copied().unwrap_or(0);
            self.prefix.push(base + self.lines_of(i));
        }
        self.clean = self.blocks.len();
        self.prefix.get(self.clean).copied().unwrap_or(0)
    }

    /// The cached line count of block `index` (0 when not laid out).
    fn lines_of(&self, index: usize) -> usize {
        self.layout
            .get(index)
            .and_then(Option::as_ref)
            .map_or(0, |c| c.lines.len())
    }

    /// The block holding laid-out line `idx` and the line's offset in it,
    /// as of the last [`Self::ensure_layout`]: a binary search over the
    /// clean prefix, then a walk over the blocks changed since.
    fn locate(&self, idx: usize) -> Option<(usize, usize)> {
        let known = self
            .clean
            .min(self.prefix.len().saturating_sub(1))
            .min(self.layout.len());
        let known_lines = self.prefix.get(known).copied().unwrap_or(0);
        let (mut block, mut base) = if idx < known_lines {
            let at = self.prefix[..=known]
                .partition_point(|&lines| lines <= idx)
                .saturating_sub(1);
            (at, self.prefix.get(at).copied().unwrap_or(0))
        } else {
            (known, known_lines)
        };
        while block < self.layout.len() {
            #[cfg(test)]
            self.walked.set(self.walked.get() + 1);
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
        while out.len() < len && block < self.layout.len() {
            #[cfg(test)]
            self.walked.set(self.walked.get() + 1);
            if let Some(Some(cached)) = self.layout.get(block) {
                let room = len - out.len();
                out.extend(
                    cached
                        .lines
                        .iter()
                        .skip(skip)
                        .take(room)
                        .map(|line| (BlockId(block), line)),
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

    /// Laid-out line `idx` of the whole transcript, as of the last
    /// [`Self::ensure_layout`] (`None` past the end or for stale blocks).
    pub fn line(&self, idx: usize) -> Option<&Line<'static>> {
        let (block, offset) = self.locate(idx)?;
        self.layout.get(block)?.as_ref()?.lines.get(offset)
    }

    /// Number of laid-out lines as of the last [`Self::ensure_layout`].
    pub fn line_count(&self) -> usize {
        let known = self
            .clean
            .min(self.prefix.len().saturating_sub(1))
            .min(self.layout.len());
        let known_lines = self.prefix.get(known).copied().unwrap_or(0);
        known_lines
            + (known..self.layout.len())
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

    /// Apply a primary-agent event. Returns the usage of a completed LLM turn, if any
    /// (a turn the provider sent no accounting for returns `None`).
    #[cfg(any(test, feature = "test-support"))]
    pub fn apply_agent_event(&mut self, event: &AgentEvent) -> Option<TurnUsage> {
        self.apply_agent_event_at(event, Instant::now())
    }

    /// [`Self::apply_agent_event`] with an explicit clock (for tests).
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
        match event {
            AgentEvent::TurnStarted { .. } => *self.stream(scope) = Stream::default(),
            AgentEvent::TurnCompleted { usage, .. } => {
                self.close_streams(scope);
                if let (Some(h), Some(usage)) = (helper, usage) {
                    let tokens = usage.input_tokens.saturating_add(usage.output_tokens);
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
                self.stream(scope).assistant = None;
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
                if stream.reasoning.is_none() && !stream.reasoning_seen && !content.is_empty() {
                    self.append_reasoning(scope, content);
                }
                self.stream(scope).reasoning = None;
            }
            AgentEvent::ToolCallRequested { id, name, args } => match helper {
                None => {
                    self.request_tool(id.clone(), name.clone(), args.as_value().clone(), 0);
                }
                Some(h) => {
                    self.close_streams(scope);
                    let preview = helper_args_preview(args.as_value());
                    self.update_fold(h.header, |f| {
                        f.calls += 1;
                        f.last = HelperLast::Tool {
                            name: name.clone(),
                            args: preview,
                        };
                    });
                    let block = self.request_tool(
                        key(id),
                        format!("{} {name}", h.tag),
                        args.as_value().clone(),
                        h.depth,
                    );
                    self.set_owner(block, scope);
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
            }
            AgentEvent::RunFailed { error_report, .. } => match helper {
                None => {
                    self.push_notice(
                        NoticeLevel::Error,
                        format!("error: {}", error_report.message),
                    );
                }
                Some(h) => {
                    let state = if error_report.class == meerkat_core::AgentErrorClass::Cancelled {
                        HelperState::Cancelled
                    } else {
                        HelperState::Failed
                    };
                    self.update_fold(h.header, |f| f.state = state);
                    let notice = self.push_notice(
                        NoticeLevel::Error,
                        format!("{} error: {}", h.tag, error_report.message),
                    );
                    self.set_owner(notice, scope);
                }
            },
            AgentEvent::RunStarted { .. } if let Some(h) = helper => {
                self.update_fold(h.header, |f| {
                    if matches!(
                        f.state,
                        HelperState::Done | HelperState::Failed | HelperState::Cancelled
                    ) {
                        f.state = HelperState::Running;
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
                self.push_notice(
                    NoticeLevel::Warn,
                    format!(
                        "⚠ budget: {budget_type:?} at {:.0}% ({used}/{limit})",
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
                self.push_notice(
                    NoticeLevel::Info,
                    format!(
                        "✓ compacted: {messages_before} → {messages_after} messages ({summary_tokens} summary tokens)"
                    ),
                );
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
            other => {
                if let Some((level, text)) = upstream_notice(other) {
                    self.push_notice(level, text);
                }
            }
        }
        None
    }

    fn push_raw(&mut self, block: Block) -> BlockId {
        let id = BlockId(self.blocks.len());
        if matches!(block, Block::Helper(_)) {
            self.helper_count += 1;
        }
        self.blocks.push(block);
        self.layout.push(None);
        self.view_layout.push(None);
        self.owner.push(None);
        id
    }

    /// The streaming state of `scope` (the session's when `None`, else
    /// the helper whose header it is), made when missing.
    fn stream(&mut self, scope: Option<BlockId>) -> &mut Stream {
        self.streams.entry(scope).or_default()
    }

    /// Append prose to `scope`'s open prose block (made, owned by the
    /// helper's header, when none is open); its reasoning block closes.
    fn append_text(&mut self, scope: Option<BlockId>, delta: &str) {
        let stream = *self.stream(scope);
        if let Some(id) = stream.assistant
            && let Some(Block::Assistant { md }) = self.block_mut(id)
        {
            md.push_str(delta);
        } else {
            let id = self.push_raw(Block::Assistant {
                md: delta.to_string(),
            });
            self.set_owner(id, scope);
            self.stream(scope).assistant = Some(id);
        }
        let stream = self.stream(scope);
        stream.reasoning = None;
        stream.text_seen = true;
    }

    /// Append reasoning to `scope`'s open reasoning block (made collapsed,
    /// owned by the helper's header, when none is open); its prose block
    /// closes.
    fn append_reasoning(&mut self, scope: Option<BlockId>, delta: &str) {
        let stream = *self.stream(scope);
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
        stream.reasoning_seen = true;
    }

    /// Close `scope`'s open prose and reasoning blocks.
    fn close_streams(&mut self, scope: Option<BlockId>) {
        if let Some(stream) = self.streams.get_mut(&scope) {
            stream.assistant = None;
            stream.reasoning = None;
        }
    }

    fn request_tool(&mut self, key: String, name: String, args: Value, depth: usize) -> BlockId {
        let note = self.pending_notes.shift_remove(&key).or_else(|| {
            key.rsplit_once('/')
                .and_then(|(_, call)| self.pending_notes.shift_remove(call))
        });
        let id = self.push(Block::Tool {
            name,
            args,
            state: ToolState::Requested,
            result_preview: None,
            diff: None,
            note,
            depth,
            collapsed: true,
        });
        self.by_tool_id.insert(key, id);
        id
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
                *result_preview = Some(result).filter(|r| !r.is_empty()).map(preview_result);
            }
        }
    }

    /// Attach a tool's result body (it arrives after the execution fact).
    fn tool_result(&mut self, key: &str, result: &str) {
        let Some(id) = self.tool_block(key) else {
            return;
        };
        if let Some(Block::Tool { result_preview, .. }) = self.block_mut(id) {
            *result_preview = Some(result).filter(|r| !r.is_empty()).map(preview_result);
        }
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
            && let Some(Block::Tool { name, state, .. }) = self.block_mut(id)
        {
            *state = ToolState::Denied {
                reason: message.to_string(),
            };
            if tool.is_empty() {
                tool.clone_from(name);
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

/// Serialized, truncated tool arguments for headers.
pub fn args_preview(args: &Value) -> String {
    let raw = serde_json::to_string(args).unwrap_or_default();
    truncate_chars(&raw, MAX_TOOL_ARGS_PREVIEW)
}

/// Whether `block` is a tool block with a result preview or a diff.
fn tool_has_output(block: &Block) -> bool {
    matches!(block, Block::Tool { result_preview, diff, .. }
        if result_preview.is_some() || diff.is_some())
        || matches!(
            block,
            Block::Shell {
                output: Some(_),
                ..
            }
        )
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
pub fn shell_output_preview(output: &str) -> Option<String> {
    let output = output.trim_end_matches('\n');
    (!output.is_empty()).then(|| preview_result(output))
}

/// Number of lines in the tool result a preview was made from (the preview
/// itself is capped at [`MAX_TOOL_RESULT_LINES`] plus a `... (N more lines)`
/// trailer, see [`preview_result`]).
pub fn result_line_count(preview: &str) -> usize {
    let shown = preview.lines().count();
    let more = preview
        .lines()
        .last()
        .and_then(|l| l.strip_prefix("... ("))
        .and_then(|l| l.strip_suffix(" more lines)"))
        .and_then(|n| n.parse::<usize>().ok());
    match more {
        Some(more) if shown == MAX_TOOL_RESULT_LINES + 1 => MAX_TOOL_RESULT_LINES + more,
        _ => shown,
    }
}

fn preview_result(result: &str) -> String {
    let total = result.lines().count();
    let mut out: Vec<&str> = result.lines().take(MAX_TOOL_RESULT_LINES).collect();
    let more;
    if total > MAX_TOOL_RESULT_LINES {
        more = format!("... ({} more lines)", total - MAX_TOOL_RESULT_LINES);
        out.push(&more);
    }
    out.join("\n")
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
        let run = t.len();
        t.push(Block::User {
            text: "check the build".into(),
        });
        t.label_triggered(run, "scheduled: nightly".into(), "check the build".into());
        assert!(matches!(&t.blocks()[0], Block::User { .. }));
        assert!(matches!(&t.blocks()[1], Block::Triggered { label, text }
            if label == "scheduled: nightly" && text == "check the build"));
        t.label_triggered(t.len(), "scheduled: other".into(), "elsewhere".into());
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
            name: "read_file".into(),
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
             ## tool read_file {\"path\":\"a.rs\"} (done in 3 ms)\nallowed by rule\n\n\
             ## tool read_file {\"path\":\"a.rs\"} (done in 3 ms)\nallowed by rule\nfn main() {}\n\n\
             ## assistant\ndone\n\n\
             ## approval: shell denied by classifier: rm -rf\n\n\
             ## notice (warning)\nturn interrupted\n"
        );
        assert!(!text.contains("secret thoughts"));
    }

    #[test]
    fn text_deltas_accumulate_into_one_assistant_block() {
        let mut t = Transcript::new();
        t.apply_agent_event(&AgentEvent::TurnStarted {
            turn_number: 0,
            assistant_message_id: None,
        });
        t.apply_agent_event(&text("Hel"));
        t.apply_agent_event(&text("lo"));
        t.apply_agent_event(&AgentEvent::TextComplete {
            content: "Hello".into(),
            assistant_message_id: None,
        });
        assert_eq!(t.blocks(), &[Block::Assistant { md: "Hello".into() }]);
    }

    #[test]
    fn text_complete_without_deltas_creates_block() {
        let mut t = Transcript::new();
        t.apply_agent_event(&AgentEvent::TurnStarted {
            turn_number: 0,
            assistant_message_id: None,
        });
        t.apply_agent_event(&AgentEvent::TextComplete {
            content: "whole".into(),
            assistant_message_id: None,
        });
        assert_eq!(t.blocks(), &[Block::Assistant { md: "whole".into() }]);
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
            Block::Tool { name, depth: 1, .. } if name == "[helper] glob"
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
            Block::Tool { name, depth: 2, .. } if name == "[inner · gpt-5.5] glob"
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
        assert!(matches!(&view[2], Block::Tool { name, .. } if name == "[scout] glob"));
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
    fn finished_helpers_leave_the_panel_when_a_turn_starts() {
        let mut t = Transcript::new();
        t.note_live_helpers([("a", "m", 1), ("b", "m", 1)]);
        let (a, b) = (t.helper_header("a").unwrap(), t.helper_header("b").unwrap());
        assert_eq!(t.panel_helpers(), vec![b, a]);
        t.note_live_helpers([("b", "m", 1)]);
        assert_eq!(t.panel_helpers(), vec![b, a]);
        t.retire_finished_helpers();
        assert_eq!(t.panel_helpers(), vec![b]);
        // Removing an earlier block shifts the panel's ids with the blocks.
        t.push(Block::User { text: "x".into() });
        assert!(t.remove_block(a).is_some());
        assert_eq!(t.panel_helpers(), vec![BlockId(b.0 - 1)]);
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

    #[test]
    fn replace_and_remove_blocks_keep_order_and_indices() {
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
        assert_eq!(t.tool_block("t1"), Some(BlockId(1)));
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
        assert_eq!(t.laid_out, 10_000);
        assert_eq!(total, t.line_count());
        let last = BlockId(t.len() - 1);
        if let Some(Block::User { text }) = t.block_mut(last) {
            text.push_str("\nmore");
        }
        let laid = t.laid_out;
        let grown = t.ensure_layout(80);
        assert_eq!(t.laid_out, laid + 1, "only the changed block");
        assert_eq!(grown, total + 1);
        t.walked.set(0);
        let lines = t.visible_lines(grown - 40, 40);
        let blocks = t.visible_blocks(grown - 40, 40);
        assert_eq!(lines.len(), 40);
        assert_eq!(blocks.len(), 40);
        assert_eq!(blocks.last(), Some(&last));
        assert!(t.walked.get() < 100, "walked {} blocks", t.walked.get());
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
        let laid = t.laid_out;
        assert_eq!(t.ensure_layout(80), grown + 1);
        assert_eq!(t.laid_out, laid + 1);
        assert_eq!(t.visible_lines(0, grown + 1).len(), grown + 1);
        t.ensure_layout(60);
        assert_eq!(t.laid_out, laid + 1 + 10_000);
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

    #[test]
    fn result_line_count_reads_the_preview_trailer() {
        assert_eq!(result_line_count("one"), 1);
        assert_eq!(result_line_count("a\nb"), 2);
        let long = (0..57)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(result_line_count(&preview_result(&long)), 57);
        let exact = (0..20)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(result_line_count(&preview_result(&exact)), 20);
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
}
