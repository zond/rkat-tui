//! Statements: kinds of effect the user allows, in their own words
//! ("building the project after the agent changed its dependencies").
//!
//! A statement is not a rule the gate matches: the classifier reads the
//! active statements in its environment ([`super::prompts`]) and they
//! clear the block rules for actions of the kind they describe, read
//! narrowly, never sending data to a host the user has not named. The
//! user adds one from `/approve` (a blocked call's `EFFECT:` sentence,
//! edited) for this session or always, or writes it in their own
//! `tui.toml` (`[auto] allow = ["…"]`, the user-level and realm files
//! only, never a project's `.rkat/tui.toml`).
//!
//! - **session**: lives in the daemon for the agent's session; its helpers'
//!   and forks' calls are judged under that session, so they have it too.
//! - **always**: written to the realm's `tui.toml` through the daemon's
//!   writer ([`crate::config::write::SettingsFile`]) and active at once.
//! - **config**: the user-level `tui.toml` (written there by "always, in
//!   every workspace") or the command line; removed from the user's file,
//!   or ending with the daemon for the command line's.
//!
//! At most [`MAX_STATEMENTS`] are active for a session, each at most
//! [`MAX_STATEMENT_CHARS`] characters; [`normalize`] strips control
//! characters and refuses an empty statement or one that only says
//! "anything".

use serde::{Deserialize, Serialize};

/// Most statements active for one session (every origin together).
pub const MAX_STATEMENTS: usize = 12;

/// Longest statement, in characters.
pub const MAX_STATEMENT_CHARS: usize = 200;

/// Where a statement comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementOrigin {
    /// Added for the agent's session (`/approve`).
    Session,
    /// Added always (`/approve`): the realm's `tui.toml`.
    Always,
    /// The user's own `tui.toml` (user level) or the command line.
    Config,
}

impl StatementOrigin {
    /// How `/permissions` names it.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Always => "always",
            Self::Config => "config",
        }
    }
}

/// A statement in force, as `/permissions` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatementRow {
    /// Its number (`/permissions` removes it by it).
    pub id: u64,
    /// The statement.
    pub text: String,
    /// Where it comes from.
    pub origin: StatementOrigin,
}

/// Why a statement was not taken.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StatementRefusal {
    /// Nothing is left once control characters and spaces are gone.
    #[error("the statement is empty: say which kind of action you allow")]
    Empty,
    /// Longer than [`MAX_STATEMENT_CHARS`].
    #[error(
        "the statement is {0} characters; at most {MAX_STATEMENT_CHARS}: name the kind of action in one short sentence"
    )]
    TooLong(usize),
    /// It names no kind of action ("anything", "do whatever you want").
    #[error(
        "the statement allows anything rather than a kind of action; name what kind (for example \"building the project after the agent changed its dependencies\")"
    )]
    TooBroad,
    /// [`MAX_STATEMENTS`] are active already.
    #[error("{MAX_STATEMENTS} statements are active already; remove one in /permissions first")]
    TooMany,
    /// The same statement is active already.
    #[error("that statement is active already")]
    Duplicate,
}

/// Words that name no kind of action by themselves: a statement made only
/// of them allows anything.
const BROAD_WORDS: &[&str] = &[
    "a",
    "action",
    "actions",
    "agent",
    "ahead",
    "all",
    "allow",
    "allowed",
    "allowing",
    "allows",
    "an",
    "and",
    "any",
    "anything",
    "approval",
    "approve",
    "are",
    "ask",
    "asking",
    "at",
    "be",
    "block",
    "blocked",
    "call",
    "calls",
    "can",
    "check",
    "checks",
    "command",
    "commands",
    "do",
    "does",
    "doing",
    "don",
    "effect",
    "effects",
    "every",
    "everything",
    "fine",
    "for",
    "go",
    "i",
    "in",
    "is",
    "it",
    "its",
    "kind",
    "kinds",
    "let",
    "may",
    "me",
    "my",
    "no",
    "not",
    "of",
    "ok",
    "okay",
    "on",
    "or",
    "permission",
    "permissions",
    "please",
    "review",
    "rule",
    "rules",
    "stuff",
    "sure",
    "t",
    "the",
    "thing",
    "things",
    "to",
    "tool",
    "tools",
    "want",
    "wants",
    "whatever",
    "whatsoever",
    "with",
    "without",
    "yes",
    "you",
];

/// `raw` as a statement: control characters gone (a line break or tab is
/// a space), runs of spaces one, trimmed; refused when empty, longer than
/// [`MAX_STATEMENT_CHARS`] or saying only "anything".
///
/// # Errors
///
/// [`StatementRefusal`] with the reason.
pub fn normalize(raw: &str) -> Result<String, StatementRefusal> {
    let spaced: String = raw
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    let text = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return Err(StatementRefusal::Empty);
    }
    let chars = text.chars().count();
    if chars > MAX_STATEMENT_CHARS {
        return Err(StatementRefusal::TooLong(chars));
    }
    let names_a_kind = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .any(|word| !BROAD_WORDS.contains(&word.to_lowercase().as_str()));
    if !names_a_kind {
        return Err(StatementRefusal::TooBroad);
    }
    Ok(text)
}

/// `text` (an `EFFECT:` line of the classifier) as a sentence the user can
/// take: [`normalize`]d, `None` when it does not pass.
#[must_use]
pub fn proposed(text: &str) -> Option<String> {
    normalize(text).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Control characters go, spaces fold, and what says only "anything"
    /// is refused with its reason.
    #[test]
    fn statements_are_cleaned_bounded_and_never_anything() {
        assert_eq!(
            normalize("  building the project\nafter the agent\tchanged its dependencies \u{7}")
                .unwrap(),
            "building the project after the agent changed its dependencies"
        );
        assert_eq!(normalize(" \n\u{1b} "), Err(StatementRefusal::Empty));
        for broad in [
            "anything",
            "Allow everything.",
            "do whatever you want",
            "any action is fine",
            "all commands, no review",
            "don't ask me",
        ] {
            assert_eq!(normalize(broad), Err(StatementRefusal::TooBroad), "{broad}");
        }
        for fine in [
            "pushing to the default branch of the project's own remote",
            "installing a package the agent chose with a global scope",
            "network requests to api.example.com",
        ] {
            assert!(normalize(fine).is_ok(), "{fine}");
        }
        let long = "x".repeat(MAX_STATEMENT_CHARS + 1);
        assert_eq!(
            normalize(&long),
            Err(StatementRefusal::TooLong(MAX_STATEMENT_CHARS + 1))
        );
        assert!(normalize(&"y".repeat(MAX_STATEMENT_CHARS)).is_ok());
        assert_eq!(proposed("anything at all"), None);
        assert!(StatementRefusal::TooMany.to_string().contains("12"));
    }
}
