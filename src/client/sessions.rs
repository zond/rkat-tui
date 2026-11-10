//! `/new`, `/resume` and `/rewind` in attach mode.
//!
//! A conversation is a session the daemon's orchestrator runs as
//! ([`crate::daemon::conversations`]): `/new` and `/resume` ask the daemon
//! to switch (`rkat/new`, `rkat/resume`), every attached client notices the
//! orchestrator's new `current_session_id` and shows it (a resumed one from
//! its committed messages, `rkat/transcript`). `/rewind` has no engine route for a member's session (see
//! [`REWIND_UNAVAILABLE`]).

use meerkat_core::SessionId;

use crate::daemon::conversations::Listed;

/// Why `/rewind` is not offered.
pub const REWIND_UNAVAILABLE: &str = "/rewind is not available yet: the agent runs in the workspace's daemon, and the engine refuses session/fork_at for a mob member's session, while its member forks (mob/fork_member, mob/fork_helper) seat the copy under a new identity rather than rewinding the conversation in place; a same-identity rewind is with the engine's maintainers as a design note (lukacf/meerkat#2126). /new starts over";

/// What the resume picker adds to a session created before the daemon.
pub const VIEW_ONLY: &str = "view only, created before the daemon";

/// The notice shown with a session created before the daemon (`viewed`):
/// its transcript is shown, the agent stays on `current`. Such a session
/// is no mob member's, and the engine refuses to seat the daemon's member
/// on it (`missing durable comms_name for resumed mob member`, pinned by
/// `attach_sessions::an_in_process_session_cannot_be_seated_on_the_member`).
#[must_use]
pub fn view_only_notice(viewed: &SessionId, current: &SessionId) -> String {
    let short = |id: &SessionId| {
        let id = id.to_string();
        id.get(..8).unwrap_or(&id).to_string()
    };
    format!(
        "showing session {} (view only): it was created before the daemon, and the engine seats the daemon's agent only on sessions it ran, so it cannot be continued; the agent stays on conversation {}, which /resume {} shows again. /export saves what is shown",
        short(viewed),
        short(current),
        current
    )
}

/// How long ago `at` (Unix seconds) was, roughly.
fn age(at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let secs = now.saturating_sub(at);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// The resume picker's rows: `(session id, label)`, last used first.
#[must_use]
pub fn picker_items(listed: &[Listed]) -> Vec<(String, String)> {
    listed
        .iter()
        .map(|row| {
            let id = row.session_id.to_string();
            let short = id.get(..8).unwrap_or(&id).to_string();
            let mut label = format!("{short} · {} ago", age(row.last_used));
            if let Some(messages) = row.messages {
                label.push_str(&format!(" · {messages} msgs"));
            }
            if let Some(title) = &row.title {
                label.push_str(&format!(" · {title}"));
            }
            if row.current {
                label.push_str(" (current)");
            }
            if row.view_only {
                label.push_str(&format!(" ({VIEW_ONLY})"));
            }
            (id, label)
        })
        .collect()
}

/// Resolve a `/resume` reference against the conversations (last used
/// first): `last` (`~`, `~0`), `~N`, a full id, or a unique id prefix.
/// `last` and `~N` count the conversations other than the current one (the
/// current one is the most recently used, so `last` would always name it):
/// `last` is the one before it. Sessions created before the daemon
/// (view only) are named by id or prefix only.
///
/// # Errors
///
/// Why nothing (or more than one conversation) matches.
pub fn resolve(reference: &str, listed: &[Listed]) -> Result<SessionId, String> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err("empty session reference".to_string());
    }
    let offset = match reference {
        "last" | "~" | "~0" => Some(0),
        s => match s.strip_prefix('~') {
            Some(n) => Some(
                n.parse::<usize>()
                    .map_err(|_| "expected ~N where N is a non-negative number".to_string())?,
            ),
            None => None,
        },
    };
    if let Some(offset) = offset {
        let others: Vec<&Listed> = listed
            .iter()
            .filter(|row| !row.current && !row.view_only)
            .collect();
        return match others.get(offset) {
            Some(row) => Ok(row.session_id.clone()),
            None if others.is_empty() => {
                Err("no other conversations in this workspace".to_string())
            }
            None => Err(format!(
                "only {} other conversation(s) exist; ~{offset} is out of range",
                others.len()
            )),
        };
    }
    let prefix = reference.to_ascii_lowercase();
    let mut matches = listed
        .iter()
        .filter(|row| row.session_id.to_string().starts_with(&prefix));
    match (matches.next(), matches.next()) {
        (Some(only), None) => Ok(only.session_id.clone()),
        (None, _) => Err(format!(
            "no conversation or earlier session of this workspace matches `{reference}`"
        )),
        (Some(_), Some(_)) => Err(format!(
            "prefix is ambiguous ({} conversations match); use more characters",
            2 + matches.count()
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn row(current: bool) -> Listed {
        Listed {
            session_id: SessionId::new(),
            last_used: 0,
            messages: Some(4),
            title: Some("fix the bug".into()),
            current,
            view_only: false,
        }
    }

    /// Sessions created before the daemon are offered by id, marked, and
    /// never named by `last` or `~N`.
    #[test]
    fn earlier_sessions_are_marked_and_named_by_id_only() {
        let mut earlier = row(false);
        earlier.view_only = true;
        let listed = vec![row(true), earlier.clone()];
        assert!(
            resolve("last", &listed)
                .unwrap_err()
                .contains("no other conversations")
        );
        let full = earlier.session_id.to_string();
        assert_eq!(resolve(&full, &listed).unwrap(), earlier.session_id);
        let items = picker_items(&listed);
        assert!(
            items[1].1.ends_with(&format!("({VIEW_ONLY})")),
            "{}",
            items[1].1
        );
        let notice = view_only_notice(&earlier.session_id, &listed[0].session_id);
        assert!(
            notice.contains("view only")
                && notice.contains(&format!(
                    "/resume {}",
                    &listed[0].session_id.to_string()[..8]
                )),
            "{notice}"
        );
    }

    #[test]
    fn references_resolve_as_in_process() {
        let listed = vec![row(true), row(false), row(false)];
        // `last` is the most recent conversation other than the current one.
        for last in ["last", "~", "~0"] {
            assert_eq!(resolve(last, &listed).unwrap(), listed[1].session_id);
        }
        assert_eq!(resolve("~1", &listed).unwrap(), listed[2].session_id);
        assert!(resolve("~2", &listed).unwrap_err().contains("out of range"));
        assert!(
            resolve("last", &listed[..1])
                .unwrap_err()
                .contains("no other conversations")
        );
        assert!(resolve("~x", &listed).is_err());
        let full = listed[1].session_id.to_string();
        assert_eq!(resolve(&full, &listed).unwrap(), listed[1].session_id);
        // A full id may name the current one (the daemon says so).
        let current = listed[0].session_id.to_string();
        assert_eq!(resolve(&current, &listed).unwrap(), listed[0].session_id);
        assert!(resolve("zzzz", &listed).unwrap_err().contains("matches"));
        assert!(resolve("", &listed).is_err());
        assert!(
            resolve("last", &[])
                .unwrap_err()
                .contains("no other conversations")
        );
    }

    #[test]
    fn picker_rows_name_the_size_title_and_the_current_one() {
        let listed = vec![row(true)];
        let items = picker_items(&listed);
        assert_eq!(items[0].0, listed[0].session_id.to_string());
        assert!(items[0].1.contains("4 msgs"), "{}", items[0].1);
        assert!(items[0].1.contains("fix the bug"));
        assert!(items[0].1.ends_with("(current)"));
    }
}
