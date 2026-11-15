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

use std::collections::HashMap;

use meerkat_core::SessionId;
use parking_lot::{Mutex, RwLock};
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

/// The statements in force: the engine's
/// (`crate::approval::ApprovalEngine::statements`). The lasting ones hold
/// for every session: the user's own `tui.toml` files' (`config`, `always`
/// for the realm's) and the ones added "always" while the daemon runs;
/// each session has its own besides. The files are written by the engine.
///
/// Locks: `lasting` and `sessions` are never held together.
#[derive(Debug)]
pub struct Statements {
    lasting: RwLock<Vec<StatementRow>>,
    sessions: Mutex<HashMap<SessionId, Vec<(u64, String)>>>,
}

impl Statements {
    /// The lasting statements `allow` (the layered settings'), numbered
    /// from 1; and the first number they leave free.
    pub(super) fn new(allow: &[(StatementOrigin, String)]) -> (Self, u64) {
        let lasting: Vec<StatementRow> = allow
            .iter()
            .zip(1u64..)
            .map(|((origin, text), id)| StatementRow {
                id,
                text: text.clone(),
                origin: *origin,
            })
            .collect();
        let first_free = u64::try_from(lasting.len()).unwrap_or(0) + 1;
        let statements = Self {
            lasting: RwLock::new(lasting),
            sessions: Mutex::new(HashMap::new()),
        };
        (statements, first_free)
    }

    /// The texts in force for `session`'s calls: the lasting ones, then
    /// the session's, at most [`MAX_STATEMENTS`].
    pub(super) fn texts_for(&self, session: &SessionId) -> Vec<String> {
        let mut texts: Vec<String> = self
            .lasting
            .read()
            .iter()
            .map(|row| row.text.clone())
            .collect();
        if let Some(own) = self.sessions.lock().get(session) {
            texts.extend(own.iter().map(|(_, text)| text.clone()));
        }
        texts.truncate(MAX_STATEMENTS);
        texts
    }

    /// The statements in force for `session`, as `/permissions` lists
    /// them: the lasting ones, then the session's.
    #[must_use]
    pub fn rows(&self, session: Option<&SessionId>) -> Vec<StatementRow> {
        let mut rows = self.lasting.read().clone();
        if let Some(own) = session.and_then(|s| self.sessions.lock().get(s).cloned()) {
            rows.extend(own.into_iter().map(|(id, text)| StatementRow {
                id,
                text,
                origin: StatementOrigin::Session,
            }));
        }
        rows
    }

    /// Add `text` (number `id`) for `session` and its helpers.
    pub(super) fn add_session(&self, session: &SessionId, id: u64, text: String) {
        self.sessions
            .lock()
            .entry(session.clone())
            .or_default()
            .push((id, text));
    }

    /// Add a lasting statement.
    pub(super) fn add_lasting(&self, row: StatementRow) {
        self.lasting.write().push(row);
    }

    /// Make the lasting statements `computed` (the layered settings', as
    /// [`crate::config::lasting::Lasting`] gives them): one held already
    /// (the same text, ignoring case) keeps its number, a new one takes
    /// one from `next_id`. Returns the texts added and the texts removed.
    pub(super) fn replace(
        &self,
        computed: &[(StatementOrigin, String)],
        mut next_id: impl FnMut() -> u64,
    ) -> (Vec<String>, Vec<String>) {
        let mut lasting = self.lasting.write();
        let rows: Vec<StatementRow> = computed
            .iter()
            .map(|(origin, text)| StatementRow {
                id: lasting
                    .iter()
                    .find(|row| row.text.eq_ignore_ascii_case(text))
                    .map_or_else(&mut next_id, |row| row.id),
                text: text.clone(),
                origin: *origin,
            })
            .collect();
        let held = |rows: &[StatementRow], text: &str| {
            rows.iter().any(|row| row.text.eq_ignore_ascii_case(text))
        };
        let added = rows
            .iter()
            .filter(|row| !held(&lasting, &row.text))
            .map(|row| row.text.clone())
            .collect();
        let removed = lasting
            .iter()
            .filter(|row| !held(&rows, &row.text))
            .map(|row| row.text.clone())
            .collect();
        *lasting = rows;
        (added, removed)
    }

    /// Remove `session`'s statement `id`: its text, if it was one.
    pub(super) fn remove_session(&self, session: &SessionId, id: u64) -> Option<String> {
        let mut sessions = self.sessions.lock();
        let own = sessions.get_mut(session)?;
        let at = own.iter().position(|(known, _)| *known == id)?;
        Some(own.remove(at).1)
    }

    /// Lasting statement `id`, if there is one.
    pub(super) fn lasting(&self, id: u64) -> Option<StatementRow> {
        self.lasting.read().iter().find(|row| row.id == id).cloned()
    }

    /// Remove lasting statement `id`.
    pub(super) fn remove_lasting(&self, id: u64) {
        self.lasting.write().retain(|known| known.id != id);
    }

    /// Forget `session`'s statements.
    pub(super) fn forget(&self, session: &SessionId) {
        self.sessions.lock().remove(session);
    }
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

    /// Replacing the lasting statements keeps the numbers of those that
    /// stay and numbers the new ones; a session's are untouched.
    #[test]
    fn replacing_the_lasting_statements_keeps_numbers_and_sessions() {
        let (statements, mut next) = Statements::new(&[
            (StatementOrigin::Config, "pushing to main".to_string()),
            (
                StatementOrigin::Always,
                "network requests to a.example".to_string(),
            ),
        ]);
        let session = SessionId::new();
        statements.add_session(&session, 50, "deleting build output".to_string());
        let (added, removed) = statements.replace(
            &[
                (StatementOrigin::Config, "Pushing to main".to_string()),
                (StatementOrigin::Config, "installing packages".to_string()),
            ],
            || {
                next += 1;
                next - 1
            },
        );
        assert_eq!(added, vec!["installing packages"]);
        assert_eq!(removed, vec!["network requests to a.example"]);
        let rows = statements.rows(Some(&session));
        let ids: Vec<(u64, &str)> = rows.iter().map(|row| (row.id, row.text.as_str())).collect();
        assert_eq!(
            ids,
            vec![
                (1, "Pushing to main"),
                (3, "installing packages"),
                (50, "deleting build output")
            ]
        );
    }
}
