//! How the main input takes the next key: one [`InputMode`] at a time, in
//! place of the separate fields it was (`mode`, `search_corpus`, `chord`,
//! `last_esc`, `quit.last_ctrl_c`). `main_key` asks it once
//! ([`InputMode::key_pressed`]) and the modes cannot be on together. The
//! Elm rule stands: it changes only inside `App::update`.
//!
//! # States
//!
//! - `Normal`: keys edit the prompt.
//! - `EscArmed(at)`: an Esc on an idle input with text; a second Esc
//!   within [`DOUBLE_ESC_WINDOW`] clears the input.
//! - `CtrlCArmed(at)`: a Ctrl-C on an idle, empty input; a second within
//!   [`DOUBLE_CTRL_C_WINDOW`] quits.
//! - `Chord(at)`: Ctrl-X was pressed; its second key (Ctrl-S, Ctrl-E, Esc)
//!   counts within [`CHORD_WINDOW`].
//! - `Search(Searching)`: a reverse history search (Ctrl-R) with the
//!   corpus it searches.
//!
//! # Transitions
//!
//! - Any key the main input takes ([`InputMode::key_pressed`]): `EscArmed`,
//!   `CtrlCArmed` and `Chord` → `Normal`, reporting whether each was still
//!   in its window; `Search` stays (the search takes the key).
//! - Esc, Ctrl-C, Ctrl-X ([`InputMode::arm_esc`], [`InputMode::arm_ctrl_c`],
//!   [`InputMode::start_chord`]): `Normal` → the armed state.
//! - Ctrl-R ([`InputMode::start_search`]) → `Search`; accepting or
//!   cancelling it ([`InputMode::end_search`]) → `Normal`.
//! - A tick ([`InputMode::expire`]): `CtrlCArmed` and `Chord` past their
//!   window → `Normal` (an armed Esc waits for the next key).
//! - A paste ([`InputMode::end_chord`]): `Chord` → `Normal`.
//!
//! # What stays beside it
//!
//! These coexist with the modes, so they are not folded in:
//!
//! - The completion menu (`keys::Completion`): rebuilt from the word under
//!   the cursor on every update; it stays open across a pending chord.
//! - The confirmation of `/quit` and Ctrl-D (`keys::QuitState::confirm`):
//!   no key disarms it, only its window.
//! - The last interrupt (`App::last_interrupt`): presses within its window
//!   belong to it, in any mode.
//! - The question whether to stop a helper (`OpenHelperView::confirm_stop`):
//!   the open helper view's own, and gone with it.
//! - A dialog's arming (`App::arming`): the dialogs' keys, not the input's.
//!
//! # What can no longer be represented
//!
//! - A pending chord, an armed Esc or an armed Ctrl-C during a history
//!   search, or two of them at once.
//! - A search corpus without a search.

use std::time::Instant;

use super::{CHORD_WINDOW, DOUBLE_CTRL_C_WINDOW, DOUBLE_ESC_WINDOW, HistorySearch};

/// How the main input takes the next key (see the module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum InputMode {
    /// Keys edit the prompt.
    #[default]
    Normal,
    /// An Esc at this moment armed the clear of the input.
    EscArmed(Instant),
    /// A Ctrl-C at this moment armed the quit.
    CtrlCArmed(Instant),
    /// Ctrl-X was pressed at this moment: a chord's second key is awaited.
    Chord(Instant),
    /// Keys drive a reverse history search (Ctrl-R).
    Search(Searching),
}

/// A reverse history search and the corpus it searches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Searching {
    /// The search.
    pub(crate) search: HistorySearch,
    /// What it searches: newest first, deduplicated.
    pub(crate) corpus: Vec<String>,
}

/// What the mode before a key made of it ([`InputMode::key_pressed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pressed {
    /// Nothing: a plain key (also an armed state past its window).
    Plain,
    /// The second Esc of Esc-Esc.
    EscArmed,
    /// The second Ctrl-C of Ctrl-C Ctrl-C.
    CtrlCArmed,
    /// A chord's second key.
    Chord,
    /// A key of the history search (the mode stays `Search`).
    Search,
}

/// Whether `at` is within `window` of `now`.
fn within(at: Instant, now: Instant, window: std::time::Duration) -> bool {
    now.saturating_duration_since(at) < window
}

impl InputMode {
    /// A key reaches the main input at `now`: an armed Esc or Ctrl-C and a
    /// pending chord end with it (back to `Normal`), and what they made of
    /// it is returned; a search stays.
    pub(crate) fn key_pressed(&mut self, now: Instant) -> Pressed {
        match std::mem::take(self) {
            Self::Normal => Pressed::Plain,
            Self::EscArmed(at) if within(at, now, DOUBLE_ESC_WINDOW) => Pressed::EscArmed,
            Self::CtrlCArmed(at) if within(at, now, DOUBLE_CTRL_C_WINDOW) => Pressed::CtrlCArmed,
            Self::Chord(at) if within(at, now, CHORD_WINDOW) => Pressed::Chord,
            Self::EscArmed(_) | Self::CtrlCArmed(_) | Self::Chord(_) => Pressed::Plain,
            Self::Search(searching) => {
                *self = Self::Search(searching);
                Pressed::Search
            }
        }
    }

    /// An Esc at `now` armed the clear of the input.
    pub(crate) fn arm_esc(&mut self, now: Instant) {
        *self = Self::EscArmed(now);
    }

    /// A Ctrl-C at `now` armed the quit.
    pub(crate) fn arm_ctrl_c(&mut self, now: Instant) {
        *self = Self::CtrlCArmed(now);
    }

    /// Ctrl-X at `now`: a chord's second key is awaited.
    pub(crate) fn start_chord(&mut self, now: Instant) {
        *self = Self::Chord(now);
    }

    /// A pending chord ends unused (a paste).
    pub(crate) fn end_chord(&mut self) {
        if let Self::Chord(_) = self {
            *self = Self::Normal;
        }
    }

    /// Start a history search over `corpus`.
    pub(crate) fn start_search(&mut self, search: HistorySearch, corpus: Vec<String>) {
        *self = Self::Search(Searching { search, corpus });
    }

    /// The search ended (accepted or cancelled).
    pub(crate) fn end_search(&mut self) {
        if let Self::Search(_) = self {
            *self = Self::Normal;
        }
    }

    /// A tick at `now`: an armed Ctrl-C or a chord past its window ends.
    pub(crate) fn expire(&mut self, now: Instant) {
        let expired = match self {
            Self::CtrlCArmed(at) => !within(*at, now, DOUBLE_CTRL_C_WINDOW),
            Self::Chord(at) => !within(*at, now, CHORD_WINDOW),
            Self::Normal | Self::EscArmed(_) | Self::Search(_) => false,
        };
        if expired {
            *self = Self::Normal;
        }
    }

    /// Whether it waits for a window to pass, so the loop ticks (an armed
    /// Ctrl-C, a chord).
    pub(crate) fn ticks(&self) -> bool {
        matches!(self, Self::CtrlCArmed(_) | Self::Chord(_))
    }

    /// Whether an Esc armed the clear of the input, at `now`.
    pub(crate) fn esc_armed(&self, now: Instant) -> bool {
        matches!(self, Self::EscArmed(at) if within(*at, now, DOUBLE_ESC_WINDOW))
    }

    /// Whether a second Ctrl-C at `now` would quit.
    pub(crate) fn ctrl_c_armed(&self, now: Instant) -> bool {
        matches!(self, Self::CtrlCArmed(at) if within(*at, now, DOUBLE_CTRL_C_WINDOW))
    }

    /// Whether a chord's second key at `now` counts.
    pub(crate) fn chord_pending(&self, now: Instant) -> bool {
        matches!(self, Self::Chord(at) if within(*at, now, CHORD_WINDOW))
    }

    /// The history search, while one runs.
    pub(crate) fn searching(&self) -> Option<&Searching> {
        match self {
            Self::Search(searching) => Some(searching),
            _ => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn search() -> InputMode {
        let corpus = vec!["one".to_string()];
        let mut mode = InputMode::Normal;
        mode.start_search(HistorySearch::start(String::new(), &corpus), corpus);
        mode
    }

    /// Any key ends an armed Esc, an armed Ctrl-C and a chord, saying
    /// whether each was still in its window; a search stays.
    #[test]
    fn a_key_ends_every_mode_but_a_search() {
        let t0 = Instant::now();
        let soon = t0 + Duration::from_millis(1);
        let late = t0 + Duration::from_mins(1);
        let cases: [(fn(&mut InputMode, Instant), Pressed); 3] = [
            (InputMode::arm_esc, Pressed::EscArmed),
            (InputMode::arm_ctrl_c, Pressed::CtrlCArmed),
            (InputMode::start_chord, Pressed::Chord),
        ];
        for (enter, pressed) in cases {
            let mut mode = InputMode::Normal;
            enter(&mut mode, t0);
            assert_eq!(mode.key_pressed(soon), pressed);
            assert_eq!(mode, InputMode::Normal);
            enter(&mut mode, t0);
            assert_eq!(mode.key_pressed(late), Pressed::Plain, "past its window");
            assert_eq!(mode, InputMode::Normal);
        }
        let mut mode = search();
        assert_eq!(mode.key_pressed(soon), Pressed::Search);
        assert!(mode.searching().is_some());
        mode.end_search();
        assert_eq!(mode, InputMode::Normal);
        assert_eq!(mode.key_pressed(soon), Pressed::Plain);
    }

    /// One mode at a time: entering one replaces another.
    #[test]
    fn entering_a_mode_leaves_the_other() {
        let t0 = Instant::now();
        let mut mode = search();
        mode.start_chord(t0);
        assert!(mode.searching().is_none() && mode.chord_pending(t0));
        mode.arm_esc(t0);
        assert!(mode.esc_armed(t0) && !mode.chord_pending(t0));
        mode.arm_ctrl_c(t0);
        assert!(mode.ctrl_c_armed(t0) && !mode.esc_armed(t0));
    }

    /// A tick ends an armed Ctrl-C and a chord past their window, not an
    /// armed Esc or a search; a paste ends a chord only; ending a search
    /// that is not on is refused.
    #[test]
    fn ticks_and_pastes_end_only_their_modes() {
        let t0 = Instant::now();
        let late = t0 + Duration::from_mins(1);
        let mut chord = InputMode::Chord(t0);
        assert!(chord.ticks());
        chord.expire(t0);
        assert_eq!(chord, InputMode::Chord(t0), "within its window");
        chord.expire(late);
        assert_eq!(chord, InputMode::Normal);
        let mut armed = InputMode::CtrlCArmed(t0);
        armed.expire(late);
        assert_eq!(armed, InputMode::Normal);
        let mut esc = InputMode::EscArmed(t0);
        assert!(!esc.ticks());
        esc.expire(late);
        assert_eq!(esc, InputMode::EscArmed(t0));
        esc.end_chord();
        esc.end_search();
        assert_eq!(esc, InputMode::EscArmed(t0));
        let mut searching = search();
        searching.expire(late);
        searching.end_chord();
        assert!(searching.searching().is_some());
        let mut pending = InputMode::Chord(t0);
        pending.end_chord();
        assert_eq!(pending, InputMode::Normal);
    }
}
