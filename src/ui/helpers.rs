//! Helpers (sub-agents) in the UI: the helper panel and its selection,
//! a helper's full view and `/helpers` (their usage is counted in the
//! daemon).

use super::{
    App, Block, BlockId, HelperTarget, HelperView, HelpersCommand, NoticeLevel, OpenHelperView,
    ScrollState,
};

impl App {
    /// Set how helper headers start: folded (default) or expanded.
    #[must_use]
    pub fn with_helper_view(mut self, view: HelperView) -> Self {
        self.transcript.set_helper_view(view);
        self
    }

    /// The helpers the panel under the input lists (see
    /// [`Transcript::panel_helpers`](crate::ui::Transcript::panel_helpers)).
    pub fn helper_panel(&self) -> Vec<BlockId> {
        self.transcript.panel_helpers()
    }

    /// The helper selected in the panel, if the panel still lists it.
    pub fn panel_selection(&self) -> Option<BlockId> {
        self.panel_selected
            .filter(|id| self.transcript.panel_helpers().contains(id))
    }

    /// The helper whose full view is open, if its header is still one.
    pub fn helper_view(&self) -> Option<BlockId> {
        self.helper_view
            .as_ref()
            .map(|view| view.header)
            .filter(|id| matches!(self.transcript.block(*id), Some(Block::Helper(_))))
    }

    /// The scroll state of the open helper view (the renderer reconciles
    /// it with the view's line count).
    pub fn helper_view_scroll(&mut self) -> Option<&mut ScrollState> {
        self.helper_view.as_mut().map(|view| &mut view.scroll)
    }

    /// Ctrl+Up (`forward == false`) / Ctrl+Down: move the panel selection,
    /// starting at the first (newest) helper and wrapping around.
    pub(super) fn move_panel_selection(&mut self, forward: bool) {
        let panel = self.transcript.panel_helpers();
        let (Some(first), Some(last)) = (panel.first().copied(), panel.last().copied()) else {
            self.panel_selected = None;
            return;
        };
        let next = match self
            .panel_selection()
            .and_then(|s| panel.iter().position(|p| *p == s))
        {
            None => first,
            Some(i) if forward => panel.get(i + 1).copied().unwrap_or(first),
            Some(i) => i
                .checked_sub(1)
                .and_then(|j| panel.get(j).copied())
                .unwrap_or(last),
        };
        self.panel_selected = Some(next);
    }

    /// Enter on an empty input with a panel selection: open that helper's
    /// full view, or close it when it is the one open.
    pub(super) fn toggle_helper_view(&mut self, header: BlockId) {
        if self.helper_view() == Some(header) {
            self.helper_view = None;
        } else {
            self.helper_view = Some(OpenHelperView {
                header,
                scroll: ScrollState::default(),
            });
        }
        self.clear_selection();
    }

    /// The scroll state the scroll keys move: the open helper view's, else
    /// the transcript's.
    pub(super) fn active_scroll(&mut self) -> &mut ScrollState {
        if self.helper_view().is_some()
            && let Some(view) = self.helper_view.as_mut()
        {
            return &mut view.scroll;
        }
        &mut self.scroll
    }

    /// Expand or fold the helper header `id`; expanding stops following new
    /// output so the header stays where it is.
    pub(super) fn toggle_helper(&mut self, id: BlockId) -> bool {
        match self.transcript.toggle_helper(id) {
            Some(expanded) => {
                if expanded {
                    self.scroll.follow = false;
                }
                true
            }
            None => false,
        }
    }

    /// `/helpers`: list, expand or fold.
    pub(super) fn helpers_command(&mut self, cmd: HelpersCommand) {
        let (target, expand) = match cmd {
            HelpersCommand::List => {
                let text = self.helpers_listing();
                self.transcript.push_notice(NoticeLevel::Info, text);
                return;
            }
            HelpersCommand::Expand(target) => (target, true),
            HelpersCommand::Fold(target) => (target, false),
        };
        let ids: Vec<BlockId> = match &target {
            HelperTarget::All => self.transcript.helper_headers().map(|(id, _)| id).collect(),
            HelperTarget::Label(label) => {
                self.transcript.helper_header(label).into_iter().collect()
            }
        };
        if ids.is_empty() {
            let text = match target {
                HelperTarget::All => "no helpers yet".to_string(),
                HelperTarget::Label(label) => format!("no helper `{label}` (try /helpers)"),
            };
            self.transcript.push_notice(NoticeLevel::Warn, text);
            return;
        }
        for id in ids {
            self.transcript.set_helper_expanded(id, expand);
        }
        if expand {
            self.scroll.follow = false;
        }
    }

    /// The `/helpers` listing: one line per helper header.
    pub(super) fn helpers_listing(&self) -> String {
        use std::fmt::Write as _;
        let mut text = String::from("helpers:");
        let mut any = false;
        for (_, fold) in self.transcript.helper_headers() {
            any = true;
            let indent = "  ".repeat(fold.depth);
            let marker = if fold.expanded { "▾" } else { "▸" };
            let status = fold.status_text();
            // No spinner glyph in a static listing.
            let status = if fold.state.is_running() {
                status
                    .split_once(' ')
                    .map_or(status.as_str(), |(_, rest)| rest)
            } else {
                status.as_str()
            };
            let _ = write!(text, "\n{indent}{marker} {} {status}", fold.tag());
        }
        if any {
            text.push_str(
                "\n(/helpers expand|fold <label>|all; Ctrl+Up/Down then Enter opens one in full)",
            );
        } else {
            text.push_str(" none yet");
        }
        text
    }
}
