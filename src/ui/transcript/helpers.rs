//! Helpers (sub-agents) in the transcript: each helper's header block
//! ([`HelperFold`]), the blocks it owns, the helper panel and the helper
//! full view, and the mapping of a helper's scoped events onto them.

use std::time::Instant;

use indexmap::IndexMap;
use meerkat_core::{ScopedAgentEvent, StreamScopeFrame};
use ratatui::text::Line;
use serde_json::Value;

use super::{Block, BlockId, CachedLayout, HelperScope, Transcript};
use crate::ui::markdown::truncate_chars;
use crate::ui::render::{SPINNER, SPINNER_FRAME_MS, block_lines};

/// Maximum characters of a helper header's last-activity preview.
pub const MAX_HELPER_PREVIEW: usize = 60;

/// How long a helper that ended stays on the helper panel, dim with its
/// ending word, before it leaves it.
pub const PANEL_LINGER: std::time::Duration = std::time::Duration::from_secs(10);

pub use crate::config::settings::HelperView;

/// Lifecycle state of a helper, as its header shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperState {
    /// The helper is working.
    Running,
    /// Its run completed (or it left the running helpers).
    Done,
    /// Its run failed.
    Failed,
    /// Its run was cancelled.
    Cancelled,
}

use crate::mobs::{HelperContext, HelperLife};

/// What a helper is, from the call that started it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperKind {
    /// Started by `assign` with its two choices (or, in a conversation an
    /// earlier build stored, by `fork_off`: a kept fork; by `spawn_member`,
    /// `spawn_many_members`: a kept fresh helper). A member of a mob the
    /// agent created (`mob_spawn_member`) is a kept fresh one too.
    Started(HelperContext, HelperLife),
    /// A `delegate` helper (a conversation an earlier build stored: the
    /// engine's `delegate` is not offered): one task, its answer returned
    /// to the caller.
    Delegate,
}

impl HelperKind {
    /// Its words, as the panel, `/helpers`, the full view's title and
    /// `/export` show them (`fresh · single-use`, `fork · kept`).
    #[must_use]
    pub fn text(self) -> String {
        match self {
            Self::Started(context, life) => format!("{} · {}", context.word(), life.word()),
            Self::Delegate => "delegate".to_string(),
        }
    }

    /// Whether it stays seated after its runs.
    #[must_use]
    pub fn kept(self) -> bool {
        matches!(self, Self::Started(_, HelperLife::Kept))
    }

    /// The kind of helper the call of the tool `name` with `args` starts
    /// (`None`: it starts none).
    fn of_call(name: &str, args: &Value) -> Option<Self> {
        match name {
            crate::tools::ASSIGN => {
                let context = match args.get("context").and_then(Value::as_str) {
                    Some("fork") => HelperContext::Fork,
                    _ => HelperContext::Fresh,
                };
                let life = if args.get("keep").and_then(Value::as_bool) == Some(true) {
                    HelperLife::Kept
                } else {
                    HelperLife::Single
                };
                Some(Self::Started(context, life))
            }
            "delegate" => Some(Self::Delegate),
            "fork_off" => Some(Self::Started(HelperContext::Fork, HelperLife::Kept)),
            "spawn_member" | "spawn_many_members" | "mob_spawn_member" => {
                Some(Self::Started(HelperContext::Fresh, HelperLife::Kept))
            }
            _ => None,
        }
    }
}

/// The engine's opening of a `fork_off` child's first input
/// (meerkat-mob-mcp `agent_tools.rs`): `DELEGATED DURABLE FORK`, the task
/// after `Task:`, then the expected output and the fork's instructions.
const FORK_HEADER: &str = "DELEGATED DURABLE FORK";
/// The fork input's label before its task.
const FORK_TASK: &str = "\n\nTask:\n";
/// The fork input's label after its task.
const FORK_AFTER_TASK: &str = "\n\nExpected output guidance:";

/// The task of a `fork_off` child's first input (`None`: not a fork's).
fn fork_task(input: &str) -> Option<&str> {
    let rest = input.strip_prefix(FORK_HEADER)?;
    let task = rest.strip_prefix(FORK_TASK).unwrap_or(rest);
    let task = task.split(FORK_AFTER_TASK).next().unwrap_or(task);
    Some(task.trim())
}

/// What a helper did last (the running header's preview).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HelperLast {
    /// Nothing yet.
    #[default]
    Nothing,
    /// A tool call: its name and a short argument preview.
    Tool {
        /// Tool name.
        name: String,
        /// Short argument preview (the first string argument, quoted).
        args: String,
    },
    /// The first line of its latest text.
    Text(String),
}

/// A helper's header: one line standing for everything the helper did.
///
/// The helper's own blocks (its prose, reasoning, tool calls, errors, the
/// headers of helpers it spawned) stay in the transcript, owned by this
/// header; the renderer skips them while the header is folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperFold {
    /// Member identity (`scout`, `fork-1`).
    pub label: String,
    /// The model it runs on, when known.
    pub model: Option<String>,
    /// Lifecycle state.
    pub state: HelperState,
    /// Tool calls requested so far.
    pub calls: usize,
    /// Its latest activity.
    pub last: HelperLast,
    /// Whether its nested blocks are shown.
    pub expanded: bool,
    /// Tokens (input + output) of its model calls, when any were reported.
    pub usage: Option<u64>,
    /// Nesting depth (1 for a helper of the session's own agent).
    pub depth: usize,
    /// Spinner frame of a running helper.
    pub spin: usize,
    /// Whether it was live (running, or seated idle) at an update of this
    /// client: the panel listed it ([`Transcript::settle_helpers`]).
    pub was_live: bool,
    /// When it stopped being live, as this client saw it: the panel shows
    /// it dim with its ending for [`PANEL_LINGER`] after.
    pub left: Option<Instant>,
    /// What it is, when the call that started it was seen.
    pub kind: Option<HelperKind>,
    /// Its inputs shown so far (the first is its task).
    pub inputs: usize,
    /// How it ended, in plain words, when the daemon said
    /// (`stopped by you`): shown in place of its state.
    pub ended: Option<String>,
}

impl HelperFold {
    /// The helper panel's and the transcript header's text after the tag:
    /// its kind (when known) after a running helper's spinner, then its
    /// status (`⠋ fresh · single-use · running · 2 tool calls`).
    #[must_use]
    pub fn panel_status_text(&self) -> String {
        let status = self.status_text();
        let Some(kind) = self.kind else {
            return status;
        };
        let kind = kind.text();
        match status.split_once(' ') {
            Some((glyph, rest)) if SPINNER.contains(&glyph.chars().next().unwrap_or(' ')) => {
                format!("{glyph} {kind} · {rest}")
            }
            _ => format!("{kind} · {status}"),
        }
    }

    /// The full view's title text after the tag: its kind, state and
    /// progress (`fresh · single-use · finished · 3 tool calls · 2.1k
    /// tokens`). A kept helper whose run is done is idle: it stays until it
    /// is retired.
    #[must_use]
    pub fn view_status_text(&self) -> String {
        let status = self.status_text();
        let state = self.state.text();
        let shown = match (self.state, self.kind) {
            (HelperState::Done, Some(kind)) if kind.kept() => "idle",
            (HelperState::Done, _) => "finished",
            _ => state,
        };
        let status = status.replacen(state, shown, 1);
        match self.kind {
            Some(kind) => format!("{} · {status}", kind.text()),
            None => status,
        }
    }
}

/// A helper header's preview of a call's arguments: the first string
/// argument, quoted and shortened (`"fn resolve"`), else the serialized
/// arguments shortened; empty for no arguments.
pub(super) fn helper_args_preview(args: &Value) -> String {
    /// Arguments that say most about a call, checked first (a JSON object's
    /// own key order is not kept).
    const TELLING: [&str; 6] = ["command", "pattern", "query", "path", "file_path", "url"];
    let first_str = args.as_object().and_then(|map| {
        TELLING
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str))
            .or_else(|| map.values().find_map(Value::as_str))
    });
    match first_str {
        Some(s) => {
            let line = s.lines().next().unwrap_or_default();
            format!("\"{}\"", truncate_chars(line, MAX_HELPER_PREVIEW))
        }
        None if args.as_object().is_some_and(serde_json::Map::is_empty) || args.is_null() => {
            String::new()
        }
        None => truncate_chars(
            &serde_json::to_string(args).unwrap_or_default(),
            MAX_HELPER_PREVIEW,
        ),
    }
}

impl Transcript {
    /// Set how new helper headers start (folded or expanded).
    pub fn set_helper_view(&mut self, view: HelperView) {
        self.helper_view = view;
    }

    /// Remember that the helper `label` runs on `model` (its blocks name it).
    pub fn note_helper_model(&mut self, label: &str, model: &str) {
        self.helper_models
            .insert(label.to_string(), model.to_string());
        if let Some(id) = self.helper_folds.get(label).copied()
            && let Some(Block::Helper(fold)) = self.block(id)
            && fold.model.as_deref() != Some(model)
            && let Some(Block::Helper(fold)) = self.block_mut(id)
        {
            fold.model = Some(model.to_string());
        }
    }

    /// The helpers running now, `(label, model, depth)`: notes their models,
    /// gives a newly spawned top-level helper its header at once, marks a
    /// helper that is no longer listed done, and lets a helper whose label
    /// is spawned again after it finished get a fresh header.
    pub fn note_live_helpers<'a>(
        &mut self,
        live: impl IntoIterator<Item = (&'a str, &'a str, usize)>,
    ) {
        let mut now_live = Vec::new();
        for (label, model, depth) in live {
            self.note_helper_model(label, model);
            let fresh = !self.live_helpers.iter().any(|l| l == label);
            if fresh
                && let Some(id) = self.helper_folds.get(label).copied()
                && matches!(self.block(id), Some(Block::Helper(f)) if !f.state.is_running())
            {
                self.helper_folds.shift_remove(label);
            }
            if depth == 1 {
                self.ensure_fold(&[label]);
            }
            now_live.push(label.to_string());
        }
        let gone: Vec<BlockId> = self
            .live_helpers
            .iter()
            .filter(|label| !now_live.contains(label))
            .filter_map(|label| self.helper_folds.get(label.as_str()).copied())
            .collect();
        for id in gone {
            self.update_fold(id, |fold| {
                if fold.state.is_running() {
                    fold.state = HelperState::Done;
                }
            });
        }
        self.live_helpers = now_live;
    }

    /// Whether a helper header spins (a helper runs).
    pub fn helpers_running(&self) -> bool {
        self.helper_clock.is_some()
            && self
                .helper_folds
                .values()
                .any(|id| matches!(self.block(*id), Some(Block::Helper(f)) if f.state.is_running()))
    }

    /// Advance the spinner of running helper headers. Returns whether any
    /// header changed.
    pub fn tick_helpers(&mut self, now: Instant) -> bool {
        let Some(clock) = self.helper_clock else {
            return false;
        };
        let elapsed = now.saturating_duration_since(clock).as_millis();
        let frame = (elapsed / SPINNER_FRAME_MS) as usize % SPINNER.len();
        let mut changed = false;
        for i in 0..self.helper_folds.len() {
            let Some((_, id)) = self.helper_folds.get_index(i) else {
                continue;
            };
            let id = *id;
            if matches!(self.block(id), Some(Block::Helper(f)) if f.state.is_running() && f.spin != frame)
            {
                self.update_fold(id, |fold| fold.spin = frame);
                changed = true;
            }
        }
        changed
    }

    /// The helper headers, in transcript order.
    pub fn helper_headers(&self) -> impl Iterator<Item = (BlockId, &HelperFold)> {
        self.iter().filter_map(|(id, b)| match b {
            Block::Helper(fold) => Some((id, fold)),
            _ => None,
        })
    }

    /// The helper headers on screen (not inside a folded helper), in
    /// transcript order.
    #[cfg(any(test, feature = "test-support"))]
    pub fn visible_helper_headers(&self) -> impl Iterator<Item = BlockId> + '_ {
        self.helper_headers()
            .map(|(id, _)| id)
            .filter(|id| !self.is_hidden(*id))
    }

    /// The newest header of the helper `label`.
    pub fn helper_header(&self, label: &str) -> Option<BlockId> {
        self.helper_headers()
            .filter(|(_, fold)| fold.label == label)
            .map(|(id, _)| id)
            .last()
    }

    /// The labels of the helper `header` and the helpers above it,
    /// outermost first (empty: `header` is no helper header).
    pub fn helper_path(&self, header: BlockId) -> Vec<String> {
        let mut path = Vec::new();
        let mut at = Some(header);
        while let Some(id) = at {
            if let Some(Block::Helper(fold)) = self.block(id) {
                path.push(fold.label.clone());
            }
            at = self.owner_of(id);
        }
        path.reverse();
        path
    }

    /// Whether the transcript has any helper header.
    #[cfg(any(test, feature = "test-support"))]
    pub fn has_helpers(&self) -> bool {
        self.helper_count > 0
    }

    /// An update at `now`: each helper that is live is noted so, and one
    /// that was and is no longer gets the time it left (the panel shows it
    /// dim for [`PANEL_LINGER`] after). Turns do not matter.
    pub fn settle_helpers(&mut self, now: Instant) {
        self.panel_now = Some(now);
        let changes: Vec<(BlockId, bool)> = self
            .helper_headers()
            .filter_map(|(id, fold)| {
                let live = self.helper_live(fold);
                let changes = if live {
                    !fold.was_live || fold.left.is_some()
                } else {
                    fold.was_live && fold.left.is_none()
                };
                changes.then_some((id, live))
            })
            .collect();
        for (id, live) in changes {
            self.update_fold(id, |fold| {
                if live {
                    fold.was_live = true;
                    fold.left = None;
                } else {
                    fold.left = Some(now);
                }
            });
        }
    }

    /// Whether the helper `fold` that is not live is still shown on the
    /// panel at the last update's clock: it ended less than
    /// [`PANEL_LINGER`] ago.
    fn lingers(&self, fold: &HelperFold) -> bool {
        fold.left.is_some_and(|left| {
            self.panel_now
                .is_none_or(|now| now.saturating_duration_since(left) < PANEL_LINGER)
        })
    }

    /// Whether the panel shows a helper that ended and will leave it (the
    /// UI ticks until it has).
    #[must_use]
    pub fn helpers_lingering(&self) -> bool {
        self.helper_headers()
            .any(|(_, fold)| !self.helper_live(fold) && self.lingers(fold))
    }

    /// The helpers the panel lists: the live ones (running, or seated
    /// idle), and for [`PANEL_LINGER`] one that ended (`keep`, the
    /// selected row, stays while selected). Running ones first, then the
    /// idle, then the ended; each top-level helper with the helpers nested
    /// in it (in spawn order), the newest first.
    pub fn panel_helpers(&self, keep: Option<BlockId>) -> Vec<BlockId> {
        // Each top-level helper's rows, and the highest section any of
        // them is in: 0 running, 1 idle, 2 ended.
        let mut groups: IndexMap<BlockId, (u8, Vec<BlockId>)> = IndexMap::new();
        for (id, fold) in self.helper_headers() {
            let live = self.helper_live(fold);
            if !live && !self.lingers(fold) && keep != Some(id) {
                continue;
            }
            let section = match (fold.state.is_running(), live) {
                (true, true) => 0,
                (false, true) => 1,
                _ => 2,
            };
            let mut root = id;
            while let Some(up) = self.owner_of(root) {
                root = up;
            }
            let group = groups.entry(root).or_insert((u8::MAX, Vec::new()));
            group.0 = group.0.min(section);
            group.1.push(id);
        }
        let mut groups: Vec<(u8, Vec<BlockId>)> = groups.into_values().rev().collect();
        groups.sort_by_key(|(section, _)| *section);
        groups.into_iter().flat_map(|(_, rows)| rows).collect()
    }

    /// Whether `block` belongs to the helper `header` and shows in its full
    /// view: no folded helper sits between them.
    pub(super) fn shown_in_view(&self, block: BlockId, header: BlockId) -> bool {
        let mut at = block;
        while let Some(up) = self.owner_of(at) {
            if up == header {
                return true;
            }
            if matches!(self.block(up), Some(Block::Helper(f)) if !f.expanded) {
                return false;
            }
            at = up;
        }
        false
    }

    /// The blocks the full view of the helper `header` shows, in order: its
    /// prose, reasoning, tool calls, notes and the headers of the helpers
    /// it spawned (with their blocks while expanded).
    pub fn helper_view_blocks(&self, header: BlockId) -> Vec<BlockId> {
        self.iter()
            .map(|(id, _)| id)
            .filter(|id| *id > header && self.shown_in_view(*id, header))
            .collect()
    }

    /// The laid-out lines of the full view of the helper `header` at
    /// `width` (without its title line).
    #[cfg(any(test, feature = "test-support"))]
    pub fn helper_view_lines(&mut self, header: BlockId, width: u16) -> Vec<Line<'static>> {
        self.helper_view_rows(header, width).0
    }

    /// [`Self::helper_view_lines`], with the block each line belongs to
    /// (what a click or a drag on a row of the view reaches).
    pub fn helper_view_rows(
        &mut self,
        header: BlockId,
        width: u16,
    ) -> (Vec<Line<'static>>, Vec<BlockId>) {
        let ids = self.helper_view_blocks(header);
        let mut lines = Vec::new();
        let mut blocks = Vec::new();
        for id in ids {
            let Some(entry) = self.entry_mut(id) else {
                continue;
            };
            if entry
                .view_layout
                .as_ref()
                .is_none_or(|cached| cached.width != width)
            {
                entry.view_layout = Some(CachedLayout {
                    width,
                    lines: block_lines(&entry.block, width),
                });
            }
            if let Some(cached) = &entry.view_layout {
                lines.extend(cached.lines.iter().cloned());
                blocks.extend(std::iter::repeat_n(id, cached.lines.len()));
            }
        }
        (lines, blocks)
    }

    /// Expand or fold the helper header `id`. Returns the new `expanded`
    /// state, or `None` when `id` is not a helper header.
    pub fn toggle_helper(&mut self, id: BlockId) -> Option<bool> {
        let expanded = match self.block(id) {
            Some(Block::Helper(fold)) => !fold.expanded,
            _ => return None,
        };
        self.set_helper_expanded(id, expanded);
        Some(expanded)
    }

    /// Expand (`true`) or fold the helper header `id`. Returns whether `id`
    /// is a helper header.
    pub fn set_helper_expanded(&mut self, id: BlockId, expanded: bool) -> bool {
        if !matches!(self.block(id), Some(Block::Helper(_))) {
            return false;
        }
        self.update_fold(id, |fold| fold.expanded = expanded);
        // Every block inside the helper appears or disappears.
        let inside: Vec<bool> = self
            .iter()
            .map(|(block, _)| self.owned_by(block, id))
            .collect();
        for (entry, inside) in self.entries.iter_mut().zip(inside) {
            if inside {
                entry.layout = None;
            }
        }
        true
    }

    /// Whether `block` sits (at any depth) inside the helper header `header`.
    pub(super) fn owned_by(&self, block: BlockId, header: BlockId) -> bool {
        let mut at = block;
        while let Some(up) = self.owner_of(at) {
            if up == header {
                return true;
            }
            at = up;
        }
        false
    }

    /// The header of the helper `path` (outermost first), made (with the
    /// headers of the helpers above it) when missing.
    pub(super) fn ensure_fold(&mut self, path: &[&str]) -> BlockId {
        let Some((label, parents)) = path.split_last() else {
            return BlockId(usize::MAX);
        };
        if let Some(id) = self.helper_folds.get(*label) {
            return *id;
        }
        let parent = (!parents.is_empty()).then(|| self.ensure_fold(parents));
        // The helper's own block: it parts no other scope's prose.
        let id = self.push_raw(Block::Helper(HelperFold {
            label: (*label).to_string(),
            model: self.helper_model(label).map(str::to_string),
            state: HelperState::Running,
            calls: 0,
            last: HelperLast::Nothing,
            expanded: self.helper_view == HelperView::Expanded,
            usage: None,
            depth: path.len(),
            spin: 0,
            was_live: false,
            left: None,
            kind: self.helper_kinds.get(*label).copied(),
            inputs: 0,
            ended: None,
        }));
        self.set_owner(id, parent);
        self.helper_folds.insert((*label).to_string(), id);
        if self.helper_clock.is_none() {
            self.helper_clock = Some(Instant::now());
        }
        id
    }

    pub(super) fn set_owner(&mut self, id: BlockId, owner: Option<BlockId>) {
        // Whether it is hidden may have changed: its layout goes.
        if let Some(index) = self.index_of(id) {
            self.touch(index);
            let entry = &mut self.entries[index];
            entry.owner = owner;
            entry.layout = None;
        }
    }

    pub(super) fn update_fold(&mut self, id: BlockId, f: impl FnOnce(&mut HelperFold)) {
        if let Some(Block::Helper(fold)) = self.block_mut(id) {
            f(fold);
        }
    }

    /// The name of the agent this transcript is: who gave its helpers
    /// their tasks.
    pub fn set_agent_name(&mut self, name: Option<String>) {
        self.agent_name = name;
    }

    /// The daemon listed the helpers seated for this conversation now
    /// (its whole tree, by label): the whole list, each time one is
    /// seated or leaves.
    pub fn seated_helpers(&mut self, labels: impl IntoIterator<Item = String>) {
        self.seated_helpers = Some(labels.into_iter().collect());
    }

    /// Whether the helper `fold` is live, from what the daemon knows of
    /// it: running, or seated (a message reaches it). One the daemon said
    /// ended (finished, failed, stopped, retired) is not, nor one its last
    /// list of the seated helpers does not name. How its run ended says
    /// nothing of it. A helper of a mob the agent made itself
    /// (`mob_spawn_member`) is live until the daemon says it ended.
    #[must_use]
    pub fn helper_live(&self, fold: &HelperFold) -> bool {
        let going = fold.state.is_running() || fold.ended.is_none();
        if self.elsewhere.contains(&fold.label) {
            return going;
        }
        going
            && self
                .seated_helpers
                .as_ref()
                .is_none_or(|seated| seated.contains(&fold.label))
    }

    /// A tool call of the agent or of a helper: one that starts helpers
    /// says what they are (by the `member_id` of its arguments, or of each
    /// of their entries; an `assign`'s `name`).
    pub(super) fn note_spawn(&mut self, tool: &str, args: &Value) {
        let Some(kind) = HelperKind::of_call(tool, args) else {
            return;
        };
        let id = if tool == crate::tools::ASSIGN {
            "name"
        } else {
            "member_id"
        };
        let mut labels: Vec<&str> = args.get(id).and_then(Value::as_str).into_iter().collect();
        if let Some(map) = args.as_object() {
            for value in map.values().filter_map(Value::as_array) {
                labels.extend(
                    value
                        .iter()
                        .filter_map(|entry| entry.get("member_id").and_then(Value::as_str)),
                );
            }
        }
        for label in labels {
            if tool == "mob_spawn_member" {
                self.elsewhere.insert(label.to_string());
            }
            self.helper_kinds.insert(label.to_string(), kind);
            if let Some(id) = self.helper_folds.get(label).copied() {
                self.update_fold(id, |fold| fold.kind = Some(kind));
            }
        }
    }

    /// A run of the helper `helper` started on `input` (`None`: a
    /// continuation without one): the input is shown in its view as the
    /// main transcript shows a prompt. Its first is its task, given by its
    /// parent (a fork's opens with the line that it carries its parent's
    /// conversation, which is not shown again); a peer message is shown
    /// with its sender; any other later input as a message.
    pub(super) fn helper_input(&mut self, helper: &HelperScope<'_>, input: Option<&str>) {
        let Some(input) = input.map(str::trim).filter(|text| !text.is_empty()) else {
            return;
        };
        let first = matches!(self.block(helper.header), Some(Block::Helper(f)) if f.inputs == 0);
        let parent = helper.parent.as_deref().unwrap_or("its parent");
        let mut blocks = Vec::new();
        if let Some(messages) = crate::ui::peer::of_inputs(input) {
            blocks.extend(messages.into_iter().map(|(from, body)| Block::Peer {
                from: crate::ui::peer::short_name(&from).to_string(),
                text: body,
                collapsed: true,
                to: Some(super::PeerTo {
                    tag: helper.tag.clone(),
                    depth: helper.depth,
                }),
            }));
        } else if let Some(task) = fork_task(input) {
            // A fork: its life as the call said, else (a stored `fork_off`
            // child) kept.
            self.update_fold(helper.header, |f| {
                let life = match f.kind {
                    Some(HelperKind::Started(_, life)) => life,
                    _ => HelperLife::Kept,
                };
                f.kind = Some(HelperKind::Started(HelperContext::Fork, life));
            });
            blocks.push(Block::Notice {
                level: super::NoticeLevel::Info,
                text: format!("forked from {parent} with its conversation so far"),
            });
            blocks.push(Block::Task {
                label: format!("task from {parent}"),
                text: task.to_string(),
            });
        } else if first {
            blocks.push(Block::Task {
                label: format!("task from {parent}"),
                text: input.to_string(),
            });
        } else {
            blocks.push(Block::Task {
                label: "message".to_string(),
                text: input.to_string(),
            });
        }
        for block in blocks {
            self.push_scoped(Some(helper.header), block);
        }
        self.update_fold(helper.header, |f| f.inputs += 1);
    }

    /// The daemon said the helper `path` ended `how` (`stopped by you`)
    /// and who was `told`: its line says how instead of its state, and a
    /// notice in the transcript says it.
    pub fn helper_ended(&mut self, path: &[String], how: &str, told: Option<&str>) {
        let Some(label) = path.last() else {
            return;
        };
        if how == crate::daemon::protocol::HELPER_RETIRED {
            if let Some(seated) = &mut self.seated_helpers {
                seated.remove(label.as_str());
            }
            // It left its mob: shown on its line, unless its line says how
            // it ended already (`finished`, `stopped by you`).
            if let Some(id) = self.helper_folds.get(label.as_str()).copied() {
                self.update_fold(id, |fold| {
                    if fold.ended.is_none() {
                        fold.ended = Some(how.to_string());
                    }
                    if fold.state.is_running() {
                        fold.state = HelperState::Done;
                    }
                });
            }
            return;
        }
        let state = if how == "finished" {
            HelperState::Done
        } else if how.starts_with("failed") {
            HelperState::Failed
        } else {
            HelperState::Cancelled
        };
        if let Some(id) = self.helper_folds.get(label.as_str()).copied() {
            self.update_fold(id, |fold| {
                fold.ended = Some(how.to_string());
                fold.state = state;
            });
        }
        // A run the note started shows it folded already: that line says
        // the same.
        if let Some(told) = told
            && self.ending_shown(label, told)
        {
            return;
        }
        let told = told.map_or_else(
            || "; nobody is told (its parent is gone)".to_string(),
            |told| format!("; {told}"),
        );
        let level = if state == HelperState::Failed {
            super::NoticeLevel::Warn
        } else {
            super::NoticeLevel::Info
        };
        // News of a helper: it parts no prose of the agent's still
        // streaming (see the module doc).
        let text = format!("helper {} {how}{told}", self.helper_tag(label));
        self.push_raw(Block::Notice { level, text });
    }

    /// The model of the helper `label`, when known.
    pub fn helper_model(&self, label: &str) -> Option<&str> {
        self.helper_models.get(label).map(String::as_str)
    }

    /// `[label · model]`, or `[label]` while the model is unknown.
    pub(super) fn helper_tag(&self, label: &str) -> String {
        match self.helper_model(label) {
            Some(model) => format!("[{label} · {model}]"),
            None => format!("[{label}]"),
        }
    }

    /// Apply a scoped (sub-agent) event. Primary-scope events are ignored because they
    /// already arrive on the primary channel.
    #[cfg(any(test, feature = "test-support"))]
    pub fn apply_scoped_event(&mut self, scoped: &ScopedAgentEvent) {
        self.apply_scoped_event_at(scoped, Instant::now());
    }

    /// [`Self::apply_scoped_event`] with an explicit clock (for tests).
    ///
    /// The helper's blocks belong to its header (made when missing, nested
    /// in its parent helper's), which also counts its calls, its usage and
    /// its latest activity.
    pub fn apply_scoped_event_at(&mut self, scoped: &ScopedAgentEvent, now: Instant) {
        let path: Vec<&str> = scoped
            .scope_path
            .iter()
            .filter_map(|frame| match frame {
                StreamScopeFrame::MobMember { agent_identity, .. } => Some(agent_identity.as_str()),
                _ => None,
            })
            .collect();
        let Some(label) = path.last().copied() else {
            return;
        };
        let parent = match path.len() {
            0 | 1 => self.agent_name.clone(),
            n => path.get(n - 2).map(|p| (*p).to_string()),
        };
        let helper = HelperScope {
            header: self.ensure_fold(&path),
            tag: self.helper_tag(label),
            scope_id: &scoped.scope_id,
            depth: path.len(),
            parent,
        };
        self.apply_event(Some(&helper), &scoped.event, now);
    }
}
