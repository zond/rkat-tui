//! Helpers (sub-agents) in the transcript: each helper's header block
//! ([`HelperFold`]), the blocks it owns, the helper panel and the helper
//! full view, and the mapping of a helper's scoped events onto them.

use std::time::Instant;

use indexmap::IndexMap;
use meerkat_core::{ScopedAgentEvent, StreamScopeFrame};
use ratatui::text::Line;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Block, BlockId, CachedLayout, HelperScope, Transcript};
use crate::ui::markdown::truncate_chars;
use crate::ui::render::{SPINNER, SPINNER_FRAME_MS, block_lines};

/// Maximum characters of a helper header's last-activity preview.
pub const MAX_HELPER_PREVIEW: usize = 60;

/// How helper headers start: folded to one line (the default) or showing the
/// helper's nested blocks (`tui.toml` `[ui] helpers`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HelperView {
    /// One header line per helper; open one to watch it.
    #[default]
    Folded,
    /// Every helper shows its nested blocks.
    Expanded,
}

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
    /// Its events could not be followed: its usage is unmeasured.
    Unmeasured,
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
    /// Whether the helper panel lists it after it finished: set when the
    /// header is made, cleared for finished helpers when the next turn
    /// starts ([`Transcript::retire_finished_helpers`]).
    pub this_turn: bool,
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

    /// Helper `label`'s usage could not be observed: its header says so
    /// (a header is made if it has none).
    pub fn helper_unmeasured(&mut self, label: &str) {
        let id = self.ensure_fold(&[label]);
        self.update_fold(id, |fold| {
            if matches!(fold.state, HelperState::Running | HelperState::Done) {
                fold.state = HelperState::Unmeasured;
            }
        });
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
        self.blocks.iter().enumerate().filter_map(|(i, b)| match b {
            Block::Helper(fold) => Some((BlockId(i), fold)),
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

    /// Whether the transcript has any helper header.
    #[cfg(any(test, feature = "test-support"))]
    pub fn has_helpers(&self) -> bool {
        self.helper_count > 0
    }

    /// A turn starts: the helpers that finished drop off the helper panel.
    pub fn retire_finished_helpers(&mut self) {
        let finished: Vec<BlockId> = self
            .helper_headers()
            .filter(|(_, fold)| fold.this_turn && !fold.state.is_running())
            .map(|(id, _)| id)
            .collect();
        for id in finished {
            self.update_fold(id, |fold| fold.this_turn = false);
        }
    }

    /// The helpers the panel lists (running, or finished during the current
    /// turn): the newest top-level helper first, each followed by the
    /// helpers nested in it, in spawn order.
    pub fn panel_helpers(&self) -> Vec<BlockId> {
        let mut groups: IndexMap<BlockId, Vec<BlockId>> = IndexMap::new();
        for (id, fold) in self.helper_headers() {
            if !fold.in_panel() {
                continue;
            }
            let mut root = id;
            while let Some(Some(up)) = self.owner.get(root.0) {
                root = *up;
            }
            groups.entry(root).or_default().push(id);
        }
        groups.into_values().rev().flatten().collect()
    }

    /// Whether `block` belongs to the helper `header` and shows in its full
    /// view: no folded helper sits between them.
    pub(super) fn shown_in_view(&self, block: BlockId, header: BlockId) -> bool {
        let mut at = block;
        while let Some(Some(up)) = self.owner.get(at.0) {
            if *up == header {
                return true;
            }
            if matches!(self.block(*up), Some(Block::Helper(f)) if !f.expanded) {
                return false;
            }
            at = *up;
        }
        false
    }

    /// The blocks the full view of the helper `header` shows, in order: its
    /// prose, reasoning, tool calls, notes and the headers of the helpers
    /// it spawned (with their blocks while expanded).
    pub fn helper_view_blocks(&self, header: BlockId) -> Vec<BlockId> {
        (header.0 + 1..self.blocks.len())
            .map(BlockId)
            .filter(|id| self.shown_in_view(*id, header))
            .collect()
    }

    /// The laid-out lines of the full view of the helper `header` at
    /// `width` (without its title line).
    pub fn helper_view_lines(&mut self, header: BlockId, width: u16) -> Vec<Line<'static>> {
        let ids = self.helper_view_blocks(header);
        let mut lines = Vec::new();
        for id in ids {
            let stale = self
                .view_layout
                .get(id.0)
                .is_some_and(|slot| slot.as_ref().is_none_or(|c| c.width != width));
            if stale
                && let Some(block) = self.blocks.get(id.0)
                && let Some(slot) = self.view_layout.get_mut(id.0)
            {
                *slot = Some(CachedLayout {
                    width,
                    lines: block_lines(block, width),
                });
            }
            if let Some(Some(cached)) = self.view_layout.get(id.0) {
                lines.extend(cached.lines.iter().cloned());
            }
        }
        lines
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
        for i in 0..self.blocks.len() {
            if self.owned_by(BlockId(i), id)
                && let Some(slot) = self.layout.get_mut(i)
            {
                *slot = None;
            }
        }
        true
    }

    /// Whether `block` sits (at any depth) inside the helper header `header`.
    pub(super) fn owned_by(&self, block: BlockId, header: BlockId) -> bool {
        let mut at = block;
        while let Some(Some(up)) = self.owner.get(at.0) {
            if *up == header {
                return true;
            }
            at = *up;
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
        let id = self.push(Block::Helper(HelperFold {
            label: (*label).to_string(),
            model: self.helper_model(label).map(str::to_string),
            state: HelperState::Running,
            calls: 0,
            last: HelperLast::Nothing,
            expanded: self.helper_view == HelperView::Expanded,
            usage: None,
            depth: path.len(),
            spin: 0,
            this_turn: true,
        }));
        self.set_owner(id, parent);
        self.helper_folds.insert((*label).to_string(), id);
        if self.helper_clock.is_none() {
            self.helper_clock = Some(Instant::now());
        }
        id
    }

    pub(super) fn set_owner(&mut self, id: BlockId, owner: Option<BlockId>) {
        if let Some(slot) = self.owner.get_mut(id.0) {
            *slot = owner;
        }
    }

    pub(super) fn update_fold(&mut self, id: BlockId, f: impl FnOnce(&mut HelperFold)) {
        if let Some(Block::Helper(fold)) = self.block_mut(id) {
            f(fold);
        }
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
        let helper = HelperScope {
            header: self.ensure_fold(&path),
            tag: self.helper_tag(label),
            scope_id: &scoped.scope_id,
            depth: path.len(),
        };
        self.apply_event(Some(&helper), &scoped.event, now);
    }
}
