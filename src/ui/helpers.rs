//! Helpers (sub-agents) in the UI: the helper panel and its selection,
//! a helper's full view and `/helpers` (their usage is counted in the
//! daemon).

use super::transcript::HelperFold;
use super::{
    App, Block, BlockId, Effect, HelperTarget, HelperView, HelpersCommand, ListPicker, Modal,
    NoticeLevel, OpenHelperView, PickAct, PickKey, PickRow, PickerKind, ScrollState,
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
                confirm_stop: false,
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
            HelpersCommand::Pick => {
                let picker = self.helper_picker();
                self.show_modal(Modal::List(picker));
                return;
            }
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
            let _ = write!(text, "\n{}", helper_line(fold));
        }
        if any {
            text.push_str(
                "\n(/helpers picks one; /helpers expand|fold <label>|all; Ctrl+Up/Down then Enter opens one in full)",
            );
        } else {
            text.push_str(" none yet");
        }
        text
    }

    /// The `/helpers` picker: the helpers of this conversation in
    /// transcript order, nested ones indented under their parent, each
    /// as the transcript's header shows it (label, model, state, calls,
    /// tokens); Enter opens its full view, `f` folds or expands its block,
    /// `s` stops a running one after a confirmation.
    pub(super) fn helper_picker(&self) -> ListPicker {
        use ratatui::crossterm::event::KeyCode;
        let rows = self
            .transcript
            .helper_headers()
            .map(|(id, fold)| {
                let stop = if fold.state.is_running() {
                    PickKey::confirmed(
                        KeyCode::Char('s'),
                        PickAct::StopHelper(id),
                        stop_question(&fold.label),
                    )
                } else {
                    PickKey::refused(
                        KeyCode::Char('s'),
                        format!("helper {} is not running: nothing to stop", fold.label),
                    )
                };
                PickRow::new(
                    helper_line(fold),
                    vec![
                        PickKey::now(KeyCode::Enter, PickAct::ViewHelper(id)),
                        PickKey::now(KeyCode::Char('f'), PickAct::FoldHelper(id)),
                        stop,
                    ],
                )
            })
            .collect();
        ListPicker::new(
            PickerKind::Helpers,
            rows,
            "no helpers in this conversation yet (the agent starts them with delegate)",
            "↑/↓ move · Enter full view · f fold/expand in the transcript · s stop · Esc close",
        )
    }

    /// Stop the helper `header` (its labels from the transcript): the
    /// daemon cancels its run and retires it, and tells its parent.
    pub(super) fn stop_helper(&mut self, header: BlockId, fx: &mut Vec<Effect>) {
        let path = self.transcript.helper_path(header);
        if path.is_empty() {
            return;
        }
        if self.helper_view() == Some(header) {
            self.helper_view = None;
        }
        fx.push(Effect::StopHelper(path));
    }

    /// Esc in the full view of a running helper asks whether to stop it
    /// (`true`); a helper that is not running has nothing to stop.
    pub(super) fn ask_stop_in_view(&mut self) -> bool {
        let Some(header) = self.helper_view() else {
            return false;
        };
        let running =
            matches!(self.transcript.block(header), Some(Block::Helper(f)) if f.state.is_running());
        if running && let Some(view) = self.helper_view.as_mut() {
            view.confirm_stop = true;
        }
        running
    }

    /// The question the full view of a running helper asks on Esc, when
    /// it does.
    #[must_use]
    pub fn stop_confirmation(&self) -> Option<String> {
        let view = self.helper_view.as_ref().filter(|view| view.confirm_stop)?;
        match self.transcript.block(view.header) {
            Some(Block::Helper(fold)) => Some(stop_question(&fold.label)),
            _ => None,
        }
    }

    /// A key while the full view asks whether to stop its helper: `y`
    /// stops it, `n` or Esc keeps it running and goes back to the
    /// transcript, anything else leaves the question up.
    pub(super) fn stop_confirm_key(
        &mut self,
        code: ratatui::crossterm::event::KeyCode,
        fx: &mut Vec<Effect>,
    ) {
        use ratatui::crossterm::event::KeyCode;
        let Some(header) = self.helper_view() else {
            return;
        };
        match code {
            KeyCode::Char('y' | 'Y') => self.stop_helper(header, fx),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                self.helper_view = None;
                self.clear_selection();
            }
            _ => {}
        }
    }
}

/// What stopping the helper `label` asks.
fn stop_question(label: &str) -> String {
    format!(
        "Stop helper {label}? It cannot be undone: its run ends and it is retired (the helpers it started keep running); its parent is told."
    )
}

/// A helper header as a static line: indented by its depth, `▾` expanded
/// or `▸` folded, its tag and its status without the spinner glyph.
fn helper_line(fold: &HelperFold) -> String {
    let indent = "  ".repeat(fold.depth);
    let marker = if fold.expanded { "▾" } else { "▸" };
    let status = fold.status_text();
    let status = if fold.state.is_running() {
        status
            .split_once(' ')
            .map_or(status.as_str(), |(_, rest)| rest)
    } else {
        status.as_str()
    };
    format!("{indent}{marker} {} {status}", fold.tag())
}
