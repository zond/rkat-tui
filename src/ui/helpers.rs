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

    /// Expand or fold the helper header `id`; the line at the bottom of
    /// the screen stays there ([`Self::keep_bottom`]).
    pub(super) fn toggle_helper(&mut self, id: BlockId) -> bool {
        self.keep_bottom(Some(id));
        if self.transcript.toggle_helper(id).is_some() {
            true
        } else {
            self.fold_anchor = None;
            false
        }
    }

    /// `/helpers`: list, expand or fold.
    pub(super) fn helpers_command(&mut self, cmd: HelpersCommand) {
        let (target, expand) = match cmd {
            HelpersCommand::Pick => {
                self.helpers_all = false;
                let picker = self.helper_picker();
                self.show_modal(Modal::List(picker));
                return;
            }
            HelpersCommand::List { all } => {
                let text = self.helpers_listing(all);
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
        self.keep_bottom(ids.last().copied());
        for id in ids {
            self.transcript.set_helper_expanded(id, expand);
        }
    }

    /// The helper headers of this conversation, the live ones first (in
    /// transcript order), then the ended ones; and how many are ended.
    fn helpers_live_first(&self) -> (Vec<(BlockId, &HelperFold, bool)>, usize) {
        let (mut live, ended): (Vec<_>, Vec<_>) = self
            .transcript
            .helper_headers()
            .map(|(id, fold)| (id, fold, self.transcript.helper_live(fold)))
            .partition(|(_, _, live)| *live);
        let count = ended.len();
        live.extend(ended);
        (live, count)
    }

    /// The `/helpers list` listing: one line per live helper, then the
    /// count of the ended ones (`all`: a line for each of them too).
    pub(super) fn helpers_listing(&self, all: bool) -> String {
        use std::fmt::Write as _;
        let (helpers, ended) = self.helpers_live_first();
        if helpers.is_empty() {
            return "helpers: none yet".to_string();
        }
        let mut text = String::from("helpers:");
        let shown = helpers.iter().filter(|(_, _, live)| all || *live);
        let mut any = false;
        for (_, fold, _) in shown {
            any = true;
            let _ = write!(text, "\n{}", helper_line(fold));
        }
        if !any {
            text.push_str(" no live helpers");
        }
        if ended > 0 && !all {
            let _ = write!(
                text,
                "\n{} (/helpers list all lists them)",
                crate::text::count(ended, "ended helper")
            );
        }
        text.push_str(
            "\n(/helpers picks one; /helpers expand|fold <label>|all; Ctrl+Up/Down then Enter opens one in full)",
        );
        text
    }

    /// The `/helpers` picker: the live helpers of this conversation
    /// (running, or seated and idle) in transcript order, nested ones
    /// indented under their parent, each as the transcript's header shows
    /// it (label, model, kind, state, calls, tokens); `a` shows the ended
    /// ones too, dim, after them. Enter opens a helper's full view, `f`
    /// folds or expands its block, `s` stops a running one after a
    /// confirmation.
    pub(super) fn helper_picker(&self) -> ListPicker {
        use ratatui::crossterm::event::KeyCode;
        let all = self.helpers_all;
        let (helpers, ended) = self.helpers_live_first();
        let rows = helpers
            .into_iter()
            .filter(|(_, _, live)| all || *live)
            .map(|(id, fold, live)| {
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
                let row = PickRow::new(
                    helper_line(fold),
                    vec![
                        PickKey::now(KeyCode::Enter, PickAct::ViewHelper(id)),
                        PickKey::now(KeyCode::Char('f'), PickAct::FoldHelper(id)),
                        stop,
                    ],
                )
                .keyed(format!("{id:?}"));
                if live { row } else { row.dimmed() }
            })
            .collect();
        let toggle = match (all, ended) {
            (_, 0) => String::new(),
            (false, n) => format!(" · a: show {n} ended"),
            (true, _) => " · a: hide the ended".to_string(),
        };
        let empty = if ended == 0 {
            "no helpers in this conversation yet (the agent starts them with assign, fork_off or spawn_member)".to_string()
        } else {
            format!("no live helpers; a shows {ended} ended")
        };
        let picker = ListPicker::new(
            PickerKind::Helpers,
            rows,
            empty,
            format!(
                "↑/↓ move · Enter full view · f fold/expand in the transcript · s stop{toggle} · Esc close"
            ),
        );
        if ended == 0 {
            picker
        } else {
            picker.with_keys(vec![PickKey::now(
                KeyCode::Char('a'),
                PickAct::ShowEndedHelpers(!all),
            )])
        }
    }

    /// Stop the helper `header` (its labels from the transcript): the
    /// daemon cancels its run and retires it, and tells its parent. Where
    /// it was asked (the `/helpers` picker, the helper's full view) stays
    /// open and shows the helper's ending when the daemon's word of it
    /// arrives.
    pub(super) fn stop_helper(&mut self, header: BlockId, fx: &mut Vec<Effect>) {
        let path = self.transcript.helper_path(header);
        if path.is_empty() {
            return;
        }
        self.keep_helper();
        fx.push(Effect::StopHelper(path));
    }

    /// Ctrl-C in the full view of a running helper asks whether to stop
    /// it; of a helper that is not running it says so (a notice in the
    /// transcript, shown once the view closes) and stops nothing.
    pub(super) fn ask_stop_in_view(&mut self) {
        let Some(header) = self.helper_view() else {
            return;
        };
        match self.transcript.block(header) {
            Some(Block::Helper(fold)) if fold.state.is_running() => {
                if let Some(view) = self.helper_view.as_mut() {
                    view.confirm_stop = true;
                }
            }
            Some(Block::Helper(fold)) => {
                let text = format!("helper {} is not running: nothing to stop", fold.label);
                self.transcript.push_notice(NoticeLevel::Info, text);
            }
            _ => {}
        }
    }

    /// The question the full view of a running helper asks on Ctrl-C,
    /// when it does.
    #[must_use]
    pub fn stop_confirmation(&self) -> Option<String> {
        let view = self.helper_view.as_ref().filter(|view| view.confirm_stop)?;
        match self.transcript.block(view.header) {
            Some(Block::Helper(fold)) => Some(stop_question(&fold.label)),
            _ => None,
        }
    }

    /// A key while the full view asks whether to stop its helper: `y`
    /// stops it, `n`, Esc or Ctrl-C (a second press) keeps it running and
    /// stays in the view, anything else leaves the question up.
    pub(super) fn stop_confirm_key(
        &mut self,
        key: &ratatui::crossterm::event::KeyEvent,
        fx: &mut Vec<Effect>,
    ) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let Some(header) = self.helper_view() else {
            return;
        };
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('y' | 'Y') if !ctrl_c => self.stop_helper(header, fx),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => self.keep_helper(),
            _ if ctrl_c => self.keep_helper(),
            _ => {}
        }
    }

    /// The stop question is answered (or no longer asked): the view
    /// stays open.
    fn keep_helper(&mut self) {
        if let Some(view) = self.helper_view.as_mut() {
            view.confirm_stop = false;
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
    match fold.kind {
        Some(kind) => format!("{indent}{marker} {} {} · {status}", fold.tag(), kind.text()),
        None => format!("{indent}{marker} {} {status}", fold.tag()),
    }
}
