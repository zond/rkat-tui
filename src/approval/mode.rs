//! Permission modes: how much of the gate's work is left to the human.
//!
//! The four modes follow Claude Code's (`Shift+Tab` cycles them in this
//! order): [`PermissionMode::Manual`] (Claude Code's `default`) asks the user
//! for every action above Tier 1, [`PermissionMode::AcceptEdits`] also
//! allows in-project edits without asking, [`PermissionMode::Plan`] is
//! read-only (writes and non-read-only commands are denied with a tool error
//! that asks for a plan), and [`PermissionMode::Auto`] lets the classifier
//! review Tier-3 actions. Deny rules, sealed paths and protected-path
//! questions apply in every mode; the mode is engine-wide, so helpers act
//! under it too.
//!
//! Entering and leaving plan mode also tells the agent, as system context
//! ([`PLAN_ON_NOTE`], [`PLAN_OFF_NOTE`]), which the daemon appends
//! (`crate::daemon::agents::append_note`).

use std::fmt;

use serde::{Deserialize, Serialize};

/// How the gate treats actions that are not always allowed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionMode {
    /// Every action above Tier 1 asks the user (Claude Code's `default`).
    #[serde(rename = "default")]
    Manual,
    /// In-project, unprotected edits are allowed; everything else as
    /// [`Self::Manual`].
    #[serde(rename = "accept-edits", alias = "acceptEdits")]
    AcceptEdits,
    /// Read-only: writes, edits and commands that are not read-only are
    /// denied, so the model researches and presents a plan.
    #[serde(rename = "plan")]
    Plan,
    /// The classifier reviews Tier-3 actions (the default).
    #[default]
    #[serde(rename = "auto")]
    Auto,
}

impl PermissionMode {
    /// Every mode, in `Shift+Tab` order.
    pub const ALL: [Self; 4] = [Self::Manual, Self::AcceptEdits, Self::Plan, Self::Auto];

    /// The mode `Shift+Tab` switches to: manual → accept edits → plan →
    /// auto → manual.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Manual => Self::AcceptEdits,
            Self::AcceptEdits => Self::Plan,
            Self::Plan => Self::Auto,
            Self::Auto => Self::Manual,
        }
    }

    /// The name `/mode` and `tui.toml` (`[auto] mode`) take.
    #[must_use]
    #[cfg(test)]
    fn name(self) -> &'static str {
        match self {
            Self::Manual => "default",
            Self::AcceptEdits => "accept-edits",
            Self::Plan => "plan",
            Self::Auto => "auto",
        }
    }

    /// The status bar's label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::AcceptEdits => "accept edits",
            Self::Plan => "plan",
            Self::Auto => "auto",
        }
    }

    /// Parse a mode name (`default`, `accept-edits`, `plan`, `auto`), or the aliases `manual`,
    /// `acceptEdits` and `accept_edits` (case-insensitive).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "default" | "manual" => Some(Self::Manual),
            "accept-edits" | "acceptedits" | "accept_edits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The system context plan mode adds when it starts.
pub(crate) const PLAN_ON_NOTE: &str = "\
# Plan mode is on

The user switched to plan mode: research and plan, do not change anything. \
Use read-only tools only; file writes, edits and commands that are not \
read-only are refused while plan mode is on. When you know enough, present a \
concise, numbered plan of the changes you would make and stop: the user \
approves it before anything is changed.";

/// The system context added when plan mode ends.
pub(crate) const PLAN_OFF_NOTE: &str = "\
# Plan mode is off

The user left plan mode: the approval policy of the current permission mode \
applies again, and you may make changes.";

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::PermissionMode;

    #[test]
    fn shift_tab_cycles_manual_accept_edits_plan_auto() {
        let mut mode = PermissionMode::Manual;
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(mode);
            mode = mode.next();
        }
        assert_eq!(
            seen,
            [
                PermissionMode::Manual,
                PermissionMode::AcceptEdits,
                PermissionMode::Plan,
                PermissionMode::Auto,
                PermissionMode::Manual,
            ]
        );
        assert_eq!(
            PermissionMode::ALL.map(PermissionMode::next)[3],
            PermissionMode::Manual
        );
    }

    #[test]
    fn names_parse_and_round_trip_through_toml() {
        for mode in PermissionMode::ALL {
            assert_eq!(PermissionMode::parse(mode.name()), Some(mode));
            #[derive(serde::Serialize, serde::Deserialize)]
            struct T {
                mode: PermissionMode,
            }
            let text = toml::to_string(&T { mode }).unwrap();
            assert_eq!(text.trim(), format!("mode = \"{}\"", mode.name()));
            assert_eq!(toml::from_str::<T>(&text).unwrap().mode, mode);
        }
        assert_eq!(
            PermissionMode::parse("manual"),
            Some(PermissionMode::Manual)
        );
        assert_eq!(
            PermissionMode::parse("acceptEdits"),
            Some(PermissionMode::AcceptEdits)
        );
        assert_eq!(PermissionMode::parse("PLAN"), Some(PermissionMode::Plan));
        assert_eq!(PermissionMode::parse("yolo"), None);
        assert_eq!(PermissionMode::default(), PermissionMode::Auto);
        assert_eq!(PermissionMode::AcceptEdits.label(), "accept edits");
    }
}
