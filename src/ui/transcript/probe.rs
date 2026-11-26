//! What tests read and do to a [`Transcript`] that the UI does through
//! its keys and the renderer.

use std::time::Instant;

use meerkat_core::{AgentEvent, TurnUsage};
use ratatui::text::Line;

use super::{Block, BlockId, Transcript, foldable};

impl Transcript {
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
            None => self
                .entries
                .iter()
                .rev()
                .map(|entry| entry.id)
                .filter(|id| !self.is_hidden(*id))
                .collect(),
        }
    }

    pub fn last(&self) -> Option<&Block> {
        self.entries.last().map(|entry| &entry.block)
    }

    /// All blocks in order (a copy).
    pub fn blocks(&self) -> Vec<Block> {
        self.entries
            .iter()
            .map(|entry| entry.block.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Toggle collapse on the most recent reasoning block on screen.
    /// Returns whether one existed.
    pub fn toggle_last_reasoning(&mut self) -> bool {
        self.toggle_last_reasoning_in(None)
    }

    /// Toggle collapse on the most recent reasoning block of the transcript
    /// on screen (`None`) or of the full view of the helper `scope`.
    /// Returns whether one existed.
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

    /// Whether every foldable block is shown in full ([`Self::toggle_detail`]).
    #[must_use]
    pub fn detail(&self) -> bool {
        self.detail
    }

    /// The most recent tool block on screen with output to show or hide.
    pub fn last_tool_with_output(&self) -> Option<BlockId> {
        self.last_tool_with_output_in(None)
    }

    /// The most recent tool block with output of the transcript on screen
    /// (`None`) or of the full view of the helper `scope`.
    pub fn last_tool_with_output_in(&self, scope: Option<BlockId>) -> Option<BlockId> {
        self.scoped_blocks_rev(scope)
            .into_iter()
            .find(|id| self.block(*id).is_some_and(foldable))
    }

    /// Laid-out line `idx` of the whole transcript, as of the last
    /// [`Self::ensure_layout`] (`None` past the end or for stale blocks).
    pub fn line(&self, idx: usize) -> Option<&Line<'static>> {
        let (block, offset) = self.locate(idx)?;
        self.layout_at(block)?.lines.get(offset)
    }

    /// Apply a primary-agent event now. Returns the usage of a completed
    /// LLM turn, if any (a turn the provider sent no accounting for
    /// returns `None`).
    pub fn apply_agent_event(&mut self, event: &AgentEvent) -> Option<TurnUsage> {
        self.apply_agent_event_at(event, Instant::now())
    }
}
