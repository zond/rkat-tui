//! The realm's storage failing (a full or failing disk), and the repair of
//! an agent the engine left needing a reload.
//!
//! - **The first failure.** A turn fails because one of the engine's
//!   writes to the realm's stores failed. The engine's typed store error
//!   (`meerkat-runtime`'s `RuntimeStoreError::SqliteOperationFailed`, with
//!   SQLite's primary and extended codes) reaches a client only flattened
//!   into the run's error text (an `AgentErrorClass::Internal` or `Store`
//!   report, or a `MemberReloadRequired` reason): no typed class or code
//!   for "the disk is full" survives to an event. It is recognised from
//!   the fields that typed error writes (`primary_code=13`, SQLite's FULL;
//!   `primary_code=10`, IOERR) and from SQLite's own fixed words for those
//!   codes ([`is_storage_failure`]). The user is told in plain words
//!   ([`failed_line`]); the engine's text goes to the daemon's log and the
//!   audit log.
//! - **After it.** The engine leaves the member's runtime registration
//!   durability-degraded (a durable commit that could not be reconciled
//!   with the live shell) and refuses every send with
//!   `MobError::MemberReloadRequired` until a reload replaces it.
//!   [`reload`] runs the engine's non-destructive repair
//!   (`MobHandle::reload_member_registration`: same session, re-registered
//!   from durable truth). A daemon send refused that way is reloaded once
//!   and sent once more (`agents::steer_member`). A client's prompt or
//!   steer refused that way is reloaded by the proxy, which answers
//!   [`super::protocol::AGENT_RELOADED`]: the agent's event stream ended
//!   with its old registration, so the client opens it again and then
//!   sends the input once more (only the client can order the two). Every
//!   client of the agent is told ([`super::protocol::RELOADED`]) and opens
//!   its stream again. The step in flight when the write failed was never
//!   committed, so it is lost: the user is told so ([`RELOADED_LINE`]),
//!   or, when the reload fails too, what to do ([`reload_failed_line`]).

use std::path::Path;

use meerkat_mob::{AgentIdentity, MobError, MobId};

use super::core::Core;

/// What the user is told when an agent was reloaded after a storage
/// failure.
pub const RELOADED_LINE: &str = "the agent was reloaded after a storage failure; the step it was on when the failure came was not saved";

/// The JSON-RPC code of the engine's `MemberReloadRequired`.
#[must_use]
pub fn reload_required_code() -> i32 {
    meerkat_contracts::ErrorCode::MemberReloadRequired.jsonrpc_code()
}

/// Whether the engine's error text `text` says a write to its SQLite
/// stores failed because the disk is full or failing (the module docs:
/// the typed error's fields as it writes them, and SQLite's own words).
#[must_use]
pub fn is_storage_failure(text: &str) -> bool {
    const MARKS: [&str; 4] = [
        // SQLITE_FULL and SQLITE_IOERR as `SqliteOperationFailed` writes them.
        "primary_code=13;",
        "primary_code=10;",
        // SQLite's words for those codes (`sqlite3_errstr`).
        "database or disk is full",
        "disk I/O error",
    ];
    MARKS.iter().any(|mark| text.contains(mark))
}

/// What the user is told when a turn failed because the realm's storage
/// did (the realm's directory `realm`).
#[must_use]
pub fn failed_line(realm: &Path) -> String {
    format!(
        "the disk that holds rkat-tui's state ({}) is full or failing, so the agent's step was not saved; free space on that filesystem, then send your message again",
        realm.display()
    )
}

/// What the user is told when the reload after a storage failure failed
/// too (the realm's directory `realm`).
#[must_use]
pub fn reload_failed_line(realm: &Path) -> String {
    format!(
        "the agent could not be reloaded after a storage failure (the disk may still be full) and takes no messages until it is; free space on the filesystem that holds {}, then send again",
        realm.display()
    )
}

/// Below this many free bytes on the realm's filesystem the agents'
/// clients are warned.
pub const LOW_SPACE: u64 = 200 * 1024 * 1024;

/// How often the free space is read again.
const SPACE_EVERY: std::time::Duration = std::time::Duration::from_mins(5);

/// The bytes free to this user on the filesystem holding `path` (`None`
/// where it cannot be read).
#[must_use]
pub fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        let stat = nix::sys::statvfs::statvfs(path).ok()?;
        // Both are u64 on Linux; macOS's block count is narrower.
        #[cfg(target_os = "linux")]
        let (blocks, size) = (stat.blocks_available(), stat.fragment_size());
        #[cfg(not(target_os = "linux"))]
        let (blocks, size) = (
            u64::from(stat.blocks_available()),
            u64::from(stat.fragment_size()),
        );
        Some(blocks.saturating_mul(size))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// What the agents' clients are told when the realm's filesystem has
/// `free` bytes left, below [`LOW_SPACE`].
#[must_use]
pub fn low_space_line(realm: &Path, free: u64) -> String {
    format!(
        "the disk that holds rkat-tui's state ({}) has only {} MB free; when it is full the agent cannot save its work: free some space",
        realm.display(),
        free / (1024 * 1024)
    )
}

/// Whether a reading of `free` bytes crosses below [`LOW_SPACE`] (it was
/// not low at the last reading, `was_low`): the warning goes once per
/// crossing. Returns whether it is low now.
#[must_use]
pub fn crossed_low(was_low: bool, free: Option<u64>) -> (bool, bool) {
    let low = free.is_some_and(|free| free < LOW_SPACE);
    (low, low && !was_low)
}

/// What the agents' clients are told when the helpers' bindings file
/// `file` (under the realm `realm`) could not be written (`why`), or is
/// saved again.
#[must_use]
pub fn bindings_line(realm: &Path, file: &Path, saving: &crate::mobs::bindings::Saving) -> String {
    match saving {
        crate::mobs::bindings::Saving::Unsaved(why) => format!(
            "the disk that holds rkat-tui's state ({}) is full or failing: the helpers' bookkeeping could not be saved to {} ({why}), so after a restart a finished helper may be seated again or a retired one's binding come back; rkat-tui saves it again at the next change and every few minutes: free space on that filesystem",
            realm.display(),
            file.display()
        ),
        crate::mobs::bindings::Saving::SavedAgain => format!(
            "the helpers' bookkeeping is saved again ({})",
            file.display()
        ),
    }
}

/// Tell every live agent's clients and the audit log what the helpers'
/// bindings said of their file ([`bindings_line`]). Off the caller: the
/// bindings tell it under their own lock, which the agents' table must
/// not wait on.
pub fn bindings_saving(
    agents: std::sync::Weak<super::agents::Agents>,
    saving: crate::mobs::bindings::Saving,
) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!(
            ?saving,
            "the helper bindings' state is told to no one: no runtime"
        );
        return;
    };
    runtime.spawn(async move {
        let Some(live) = agents.upgrade() else {
            return;
        };
        let core = live.core();
        let realm = core.realm.clone();
        let file = core
            .registry
            .bindings()
            .path()
            .map_or_else(|| realm.clone(), Path::to_path_buf);
        let text = bindings_line(&realm, &file, &saving);
        let (event, warn) = match &saving {
            crate::mobs::bindings::Saving::Unsaved(_) => ("storage_write_failed", true),
            crate::mobs::bindings::Saving::SavedAgain => ("storage_write_saved", false),
        };
        if warn {
            tracing::warn!(file = %file.display(), "{text}");
        } else {
            tracing::info!(file = %file.display(), "{text}");
        }
        if let Some(audit) = &core.audit {
            audit.write(&super::audit::Line::event(event, text.clone()));
        }
        for agent in live.names() {
            core.decisions.broadcast(
                super::protocol::AGENT_NOTICE,
                &super::protocol::AgentNotice {
                    agent,
                    warn,
                    text: text.clone(),
                },
            );
        }
    });
}

/// Write what the helpers' bindings file misses, off the loop (a retry
/// after a failed write; nothing when it is up to date).
async fn flush_bindings(live: &super::agents::Agents) {
    let registry = std::sync::Arc::clone(&live.core().registry);
    let flushed = tokio::task::spawn_blocking(move || registry.bindings().flush()).await;
    if let Ok(Err(err)) = flushed {
        tracing::warn!(error = %err, "the helper bindings are still not saved");
    }
}

/// Read the realm's free space at once and every [`SPACE_EVERY`]; each
/// time it falls below [`LOW_SPACE`] every live agent's clients are told
/// once ([`low_space_line`]). Each pass also writes what the helpers'
/// bindings file misses after a failed write.
pub fn watch_free_space(
    agents: std::sync::Weak<super::agents::Agents>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut low = false;
        loop {
            let Some(live) = agents.upgrade() else {
                return;
            };
            let realm = live.core().realm.clone();
            let free = tokio::task::spawn_blocking({
                let realm = realm.clone();
                move || free_bytes(&realm)
            })
            .await
            .ok()
            .flatten();
            flush_bindings(&live).await;
            let (now_low, crossed) = crossed_low(low, free);
            low = now_low;
            if crossed && let Some(free) = free {
                tracing::warn!(realm = %realm.display(), free, "the realm's filesystem is nearly full");
                let text = low_space_line(&realm, free);
                for agent in live.names() {
                    live.core().decisions.broadcast(
                        super::protocol::AGENT_NOTICE,
                        &super::protocol::AgentNotice {
                            agent,
                            warn: true,
                            text: text.clone(),
                        },
                    );
                }
            }
            drop(live);
            tokio::time::sleep(SPACE_EVERY).await;
        }
    })
}

/// Reload `member` of `mob`, whose send the engine refused with
/// `MemberReloadRequired` (`reason`, its words: logged and audited, never
/// shown). An agent's clients are told it was reloaded
/// ([`super::protocol::RELOADED`]: they open its event stream again and
/// say [`RELOADED_LINE`]), and, when `tell_failure`, that the reload
/// failed (a client whose own send it was hears it in that send's
/// answer instead); a helper's reload is logged and audited only (a send
/// that still fails goes on as any refused send to a helper does).
///
/// # Errors
///
/// The engine's refusal of the reload.
pub async fn reload(
    core: &Core,
    mob: &MobId,
    member: &AgentIdentity,
    reason: &str,
    tell_failure: bool,
) -> Result<(), MobError> {
    tracing::warn!(%mob, %member, %reason, "the engine needs the member reloaded (a storage write failed); reloading it");
    let agent = (mob.as_str() == super::MOB_ID).then(|| member.to_string());
    let audit = |detail: String| {
        if let Some(log) = &core.audit {
            let mut line = super::audit::Line::event("storage_reload", detail);
            if let Some(agent) = &agent {
                line = line.of_agent(agent);
            }
            log.write(&line);
        }
    };
    audit(format!("{member} of {mob}: reload required: {reason}"));
    let result = reload_registration(core, mob, member).await;
    match &result {
        Ok(()) => {
            tracing::info!(%mob, %member, "the member was reloaded");
            audit(format!("{member} of {mob}: reloaded"));
            if let Some(agent) = agent
                && let Some(session_id) = core.conversations.current(&agent)
            {
                core.decisions.broadcast(
                    super::protocol::RELOADED,
                    &super::protocol::SwitchedNote { agent, session_id },
                );
            }
        }
        Err(err) => {
            tracing::warn!(%mob, %member, error = %err, "the member's reload failed");
            audit(format!("{member} of {mob}: reload failed: {err}"));
            if let Some(agent) = agent.filter(|_| tell_failure) {
                core.decisions.broadcast(
                    super::protocol::AGENT_NOTICE,
                    &super::protocol::AgentNotice {
                        agent,
                        warn: true,
                        text: reload_failed_line(&core.realm),
                    },
                );
            }
        }
    }
    result
}

/// The engine's reload of `member`'s registration (a test may make it
/// fail).
async fn reload_registration(
    core: &Core,
    mob: &MobId,
    member: &AgentIdentity,
) -> Result<(), MobError> {
    #[cfg(any(test, feature = "test-support"))]
    if core
        .reload_faults
        .fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |left| left.checked_sub(1),
        )
        .is_ok()
    {
        return Err(MobError::Internal(
            "a reload failed on purpose (test)".into(),
        ));
    }
    core.state
        .handle_for(mob)
        .await?
        .reload_member_registration(member)
        .await
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The texts from real use (paths invented) are storage failures;
    /// other failures are not, and the plain lines carry none of the
    /// engine's words.
    #[test]
    fn a_full_or_failing_disk_is_recognised_and_said_plainly() {
        for text in [
            "Internal error: failed to load head-canonical runtime authority for intra-turn checkpoint of session 5f0c: SQLite operation failed: disk I/O error; primary_code=10; extended_code=4874; path=/data/realm/sessions.sqlite3; operation=begin; caller=src/x.rs:1:1",
            "member main-ab12 requires a runtime reload before it can accept work: durability reload required after `lifecycle_commit`: stop persist failed: Store write failed: database or disk is full",
            "SQLite operation failed: database or disk is full; primary_code=13; extended_code=13; path=/data/realm/x.sqlite3; operation=commit; caller=a.rs:1:1",
        ] {
            assert!(is_storage_failure(text), "{text}");
        }
        for text in [
            "Run was cancelled",
            "rate limited by the provider",
            "SQLite operation failed: database is locked; primary_code=5; extended_code=5",
            "primary_code=130; something else",
        ] {
            assert!(!is_storage_failure(text), "{text}");
        }
        let realm = Path::new("/data/rkat/workspaces/ab12");
        for line in [
            failed_line(realm),
            reload_failed_line(realm),
            RELOADED_LINE.to_string(),
        ] {
            for raw in [
                "SQLite",
                "primary_code",
                "reload required",
                "durability",
                "-32029",
            ] {
                assert!(!line.contains(raw), "{raw}: {line}");
            }
        }
        assert!(failed_line(realm).contains("/data/rkat/workspaces/ab12"));
        assert!(failed_line(realm).contains("free space"));
        assert!(reload_failed_line(realm).contains("/data/rkat/workspaces/ab12"));
        assert_eq!(reload_required_code(), -32029);
    }

    /// The warning goes once per crossing below the limit, again after the
    /// space came back; an unreadable filesystem warns of nothing.
    #[test]
    fn low_space_is_told_once_per_crossing() {
        let mb = 1024 * 1024;
        let mut low = false;
        let mut told = Vec::new();
        for free in [
            Some(900 * mb),
            Some(150 * mb),
            Some(120 * mb),
            None,
            Some(500 * mb),
            Some(10 * mb),
        ] {
            let (now, crossed) = crossed_low(low, free);
            low = now;
            told.push(crossed);
        }
        assert_eq!(told, [false, true, false, false, false, true]);
        assert!(free_bytes(Path::new("/")).is_some_and(|free| free > 0));
        let line = low_space_line(Path::new("/data/rkat/workspaces/ab12"), 150 * mb);
        assert!(
            line.contains("150 MB free") && line.contains("/data/rkat/workspaces/ab12"),
            "{line}"
        );
    }
}
