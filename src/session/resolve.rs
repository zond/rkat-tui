//! Resolve user-facing session references (`last`, `~N`, full id, id prefix)
//! against the realm's sessions as the session service lists them
//! ([`SessionService::list`], the listing `rkat`'s
//! `resolve_flexible_session_id` reads too).

use std::cmp::Reverse;

use meerkat_core::SessionId;
use meerkat_core::service::{SessionQuery, SessionService, SessionSummary};

use crate::error::TuiError;

/// Resolve `reference` to a session id.
///
/// - `last`, `~`, `~0`: the most recently updated session
/// - `~N`: the N-th most recently updated session (0-based)
/// - a full UUID: that id (existence is checked when the session is loaded)
/// - anything else: a unique id prefix
///
/// # Errors
///
/// [`TuiError::SessionRef`] when nothing (or more than one session) matches,
/// [`TuiError::Session`] when listing fails.
pub async fn resolve_session_ref(
    service: &dyn SessionService,
    reference: &str,
) -> Result<SessionId, TuiError> {
    let reference = reference.trim();
    let fail = |reason: String| TuiError::SessionRef {
        reference: reference.to_string(),
        reason,
    };
    if reference.is_empty() {
        return Err(fail("empty session reference".to_string()));
    }

    let offset =
        match reference {
            "last" | "~" | "~0" => Some(0),
            s => match s.strip_prefix('~') {
                Some(n) => Some(n.parse::<usize>().map_err(|_| {
                    fail("expected ~N where N is a non-negative number".to_string())
                })?),
                None => None,
            },
        };

    if let Some(offset) = offset {
        let sessions = sorted_sessions(service).await?;
        return match sessions.get(offset) {
            Some(summary) => Ok(summary.session_id.clone()),
            None if sessions.is_empty() => Err(fail("no sessions in this realm".to_string())),
            None => Err(fail(format!(
                "only {} session(s) exist; ~{offset} is out of range",
                sessions.len()
            ))),
        };
    }

    if let Ok(id) = SessionId::parse(reference) {
        return Ok(id);
    }

    let prefix = reference.to_ascii_lowercase();
    let sessions = sorted_sessions(service).await?;
    let mut matches = sessions
        .iter()
        .filter(|summary| summary.session_id.to_string().starts_with(&prefix));
    match (matches.next(), matches.next()) {
        (Some(only), None) => Ok(only.session_id.clone()),
        (None, _) => Err(fail("no session matches this id or prefix".to_string())),
        (Some(_), Some(_)) => Err(fail(format!(
            "prefix is ambiguous ({} sessions match); use more characters",
            2 + matches.count()
        ))),
    }
}

/// The realm's sessions (archived and retired ones left out, as the service
/// lists them), most recently updated first (ties: newest id first). The
/// service does not promise an order, so sort here.
///
/// # Errors
///
/// Returns [`TuiError::Session`] when the service cannot list them.
pub async fn list_sessions(service: &dyn SessionService) -> Result<Vec<SessionSummary>, TuiError> {
    sorted_sessions(service).await
}

async fn sorted_sessions(service: &dyn SessionService) -> Result<Vec<SessionSummary>, TuiError> {
    let mut sessions = service.list(SessionQuery::default()).await?;
    sessions.sort_by_key(|summary| Reverse((summary.updated_at, summary.session_id.0)));
    Ok(sessions)
}
