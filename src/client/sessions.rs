//! `/new` and `/resume`.
//!
//! A conversation is a session the daemon's agent runs as
//! ([`crate::daemon::conversations`]): `/new` and `/resume` ask the daemon
//! to switch (`rkat/new`, `rkat/resume`), every attached client notices the
//! agent's new `current_session_id` and shows it (a resumed one from
//! its committed messages, `rkat/transcript`). There is no rewind: the
//! engine refuses `session/fork_at` for a mob member's session
//! (lukacf/meerkat#2126).

use crate::daemon::conversations::Listed;

/// How long ago `at` (Unix seconds) was, roughly.
fn age(at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    crate::text::span(now.saturating_sub(at), 1)
}

/// The resume picker's rows: `(session id, label)`, last used first.
/// Another agent's conversation names that agent (`mine` is this client's).
#[must_use]
pub fn picker_items(listed: &[Listed], mine: Option<&str>) -> Vec<(String, String)> {
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
            match &row.in_use {
                Some(agent) => label.push_str(&format!(" (agent {agent} works on it)")),
                None if Some(row.agent.as_str()) != mine && !row.current => {
                    label.push_str(&format!(" (agent {}'s)", row.agent));
                }
                None => {}
            }
            (id, label)
        })
        .collect()
}

pub use crate::daemon::conversations::resolve;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use meerkat_core::SessionId;

    fn row(current: bool) -> Listed {
        Listed {
            session_id: SessionId::new(),
            agent: "main-aaaa".into(),
            in_use: None,
            last_used: 0,
            messages: Some(4),
            title: Some("fix the bug".into()),
            current,
        }
    }

    #[test]
    fn references_resolve_as_in_process() {
        let listed = vec![row(true), row(false), row(false)];
        // `last` is the most recent conversation other than the current one.
        for last in ["last", "~", "~0"] {
            assert_eq!(resolve(last, &listed, None).unwrap(), listed[1].session_id);
        }
        assert_eq!(resolve("~1", &listed, None).unwrap(), listed[2].session_id);
        assert!(
            resolve("~2", &listed, None)
                .unwrap_err()
                .contains("out of range")
        );
        assert!(
            resolve("last", &listed[..1], None)
                .unwrap_err()
                .contains("no other conversations")
        );
        assert!(resolve("~x", &listed, None).is_err());
        let full = listed[1].session_id.to_string();
        assert_eq!(resolve(&full, &listed, None).unwrap(), listed[1].session_id);
        // A full id may name the current one (the daemon says so).
        let current = listed[0].session_id.to_string();
        assert_eq!(
            resolve(&current, &listed, None).unwrap(),
            listed[0].session_id
        );
        assert!(
            resolve("zzzz", &listed, None)
                .unwrap_err()
                .contains("matches")
        );
        assert!(resolve("", &listed, None).is_err());
        assert!(
            resolve("last", &[], None)
                .unwrap_err()
                .contains("no other conversations")
        );
    }

    /// `last` and `~N` count this agent's conversations; the picker names
    /// another agent's, and the one another agent works on.
    #[test]
    fn last_counts_this_agents_conversations_and_the_picker_names_the_others() {
        let mut current = row(true);
        current.agent = "main-aaaa".into();
        let mut theirs = row(false);
        theirs.agent = "main-bbbb".into();
        let mut held = row(false);
        held.agent = "main-cccc".into();
        held.in_use = Some("main-cccc".into());
        let mut mine = row(false);
        mine.agent = "main-aaaa".into();
        let listed = vec![current, theirs, held, mine.clone()];
        assert_eq!(
            resolve("last", &listed, Some("main-aaaa")).unwrap(),
            mine.session_id
        );
        assert_eq!(
            resolve("last", &listed, None).unwrap(),
            listed[1].session_id,
            "without an agent every conversation counts"
        );
        let items = picker_items(&listed, Some("main-aaaa"));
        assert!(
            items[1].1.ends_with("(agent main-bbbb's)"),
            "{}",
            items[1].1
        );
        assert!(
            items[2].1.ends_with("(agent main-cccc works on it)"),
            "{}",
            items[2].1
        );
        assert!(!items[3].1.contains("agent"), "{}", items[3].1);
    }

    #[test]
    fn picker_rows_name_the_size_title_and_the_current_one() {
        let listed = vec![row(true)];
        let items = picker_items(&listed, None);
        assert_eq!(items[0].0, listed[0].session_id.to_string());
        assert!(items[0].1.contains("4 msgs"), "{}", items[0].1);
        assert!(items[0].1.contains("fix the bug"));
        assert!(items[0].1.ends_with("(current)"));
    }
}
