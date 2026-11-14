//! The audit log (recommendation 11 of the security review): one JSON line
//! per gate decision and per lifecycle event that changes authority,
//! appended by the daemon only.
//!
//! Where: `audit.jsonl` in the workspace's realm
//! (`<config dir>/workspaces/<id>/`, [`crate::bootstrap::realm_home`]), so
//! it follows a re-home. The realm directory is 0700 and the file 0600, the
//! daemon refuses to start when its agent shell's confinement reaches the
//! realm (`TuiError::RealmExposed`), and the agent's file tools never read
//! it (the realm is a secret root of the project root): the actor it
//! records cannot read, truncate or forge it.
//!
//! Lines (`Line`, every field but `time` and `event` optional):
//!
//! - `"decision"`: `agent`, `helper`, `session`, `run`, `tool`, `argument`
//!   (the decisive argument whole: the command, the paths, else the raw
//!   arguments), `tier`, `verdict` (`allow`/`deny`), `decider` (`rule:<id>`,
//!   `classifier`, `human:<connection>`, `human` (a Stop), `mode`,
//!   `policy`, `unattended`, `timeout`) and, for a denial, `reason`;
//! - `"mode"`, `"rule_added"`, `"rule_removed"`, `"rules_trusted"`,
//!   `"project_trusted"`, `"agent_retired"`, `"daemon_start"`,
//!   `"daemon_stop"`: `detail`, and `agent` where one is concerned.
//!
//! Rotation: a write that would take the file past [`ROTATE_BYTES`]
//! renames it to `audit.1.jsonl` first (replacing the previous one: one
//! generation is kept). A write that fails is logged, never retried, and
//! never stops the gate.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use meerkat_core::{RunId, SessionId};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::approval::audit::{AuditSink, DecisionRecord};

/// The audit log's file name, in the realm directory.
pub const AUDIT_FILE: &str = "audit.jsonl";

/// The previous generation of the audit log.
pub const ROTATED_FILE: &str = "audit.1.jsonl";

/// The size past which the log is rotated.
pub const ROTATE_BYTES: u64 = 50 * 1024 * 1024;

/// One line of the audit log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    /// When (RFC 3339, UTC, milliseconds).
    pub time: String,
    /// What: `decision` or a lifecycle event (see the module docs).
    pub event: String,
    /// The agent concerned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The helper that made the call (a decision of a helper's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper: Option<String>,
    /// The session the call was judged for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The run the call belongs to, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// The tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The decisive argument, whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument: Option<String>,
    /// The tier the call fell into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// `allow` or `deny`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// Who decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decider: Option<String>,
    /// Why it was denied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// A lifecycle event's detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The classifier's review of a decided call, when one ran (stage,
    /// verdict, rule, latency, tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<crate::approval::audit::ReviewRecord>,
}

impl Line {
    /// A lifecycle line `event` (now) with `detail`.
    #[must_use]
    pub fn event(event: &str, detail: impl Into<String>) -> Self {
        Self {
            time: now(),
            event: event.to_string(),
            detail: Some(detail.into()),
            ..Self::default()
        }
    }

    /// The same line, naming `agent`.
    #[must_use]
    pub fn of_agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }
}

/// Now, as the log writes it.
fn now() -> String {
    chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now())
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Why the audit log cannot be opened.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// The file cannot be opened or created (owner-only, not through a
    /// symbolic link).
    #[error("cannot open the audit log {path}: {source}")]
    Open {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
}

/// The open audit log.
pub struct AuditLog {
    path: PathBuf,
    rotate_at: u64,
    file: Mutex<Option<(File, u64)>>,
}

impl std::fmt::Debug for AuditLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditLog")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Open `path` for appending: created 0600, an existing file made 0600,
/// never through a symbolic link. Returns the file and its size.
fn open_append(path: &Path) -> std::io::Result<(File, u64)> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if meta.permissions().mode() & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok((file, meta.len()))
}

impl AuditLog {
    /// The audit log in `dir` (the realm directory).
    ///
    /// # Errors
    ///
    /// [`AuditError::Open`] when the file cannot be opened.
    pub fn open(dir: &Path) -> Result<Arc<Self>, AuditError> {
        Self::open_rotating_at(dir, ROTATE_BYTES)
    }

    /// [`Self::open`], rotating past `rotate_at` bytes.
    ///
    /// # Errors
    ///
    /// [`AuditError::Open`].
    pub fn open_rotating_at(dir: &Path, rotate_at: u64) -> Result<Arc<Self>, AuditError> {
        let path = dir.join(AUDIT_FILE);
        let opened = open_append(&path).map_err(|source| AuditError::Open {
            path: path.clone(),
            source,
        })?;
        Ok(Arc::new(Self {
            path,
            rotate_at,
            file: Mutex::new(Some(opened)),
        }))
    }

    /// The file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append `line`.
    pub fn write(&self, line: &Line) {
        let Ok(mut text) = serde_json::to_string(line) else {
            return;
        };
        text.push('\n');
        let mut slot = self.file.lock();
        if let Some((_, size)) = slot.as_ref()
            && *size > 0
            && *size + text.len() as u64 > self.rotate_at
        {
            *slot = None;
            let rotated = self.path.with_file_name(ROTATED_FILE);
            if let Err(err) = std::fs::rename(&self.path, &rotated) {
                tracing::warn!(error = %err, path = %self.path.display(), "the audit log was not rotated");
            }
        }
        if slot.is_none() {
            match open_append(&self.path) {
                Ok(opened) => *slot = Some(opened),
                Err(err) => {
                    tracing::warn!(error = %err, path = %self.path.display(), "the audit log cannot be opened; this line is lost");
                    return;
                }
            }
        }
        if let Some((file, size)) = slot.as_mut() {
            match file.write_all(text.as_bytes()) {
                Ok(()) => *size += text.len() as u64,
                Err(err) => {
                    tracing::warn!(error = %err, path = %self.path.display(), "an audit line was not written");
                }
            }
        }
    }
}

/// The last `n` lines of the audit log at `path` (the rotated generation
/// before it when the current one holds fewer), oldest first, keeping the
/// lines `keep` accepts. A line that does not parse is kept as it is.
///
/// # Errors
///
/// The file cannot be read (a missing rotated generation is no error).
pub fn tail(path: &Path, n: usize, keep: impl Fn(&Line) -> bool) -> std::io::Result<Vec<String>> {
    let mut lines = Vec::new();
    let rotated = path.with_file_name(ROTATED_FILE);
    for file in [rotated.as_path(), path] {
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && file == rotated => continue,
            Err(err) => return Err(err),
        };
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let kept = serde_json::from_str::<Line>(line).map_or(true, |parsed| keep(&parsed));
            if kept {
                lines.push(line.to_string());
            }
        }
    }
    let skip = lines.len().saturating_sub(n);
    Ok(lines.split_off(skip))
}

/// The audit log files of the workspace `args` names, oldest generation
/// first: those in its realm (found, never made: a workspace without a
/// realm has none).
///
/// # Errors
///
/// The workspace or its realm cannot be resolved.
pub fn files(args: &crate::Args) -> Result<Vec<PathBuf>, crate::TuiError> {
    let dir = crate::bootstrap::scope::lookup(args)?
        .2
        .map(|home| home.dir);
    Ok(dir
        .into_iter()
        .flat_map(|dir| [dir.join(ROTATED_FILE), dir.join(AUDIT_FILE)])
        .filter(|file| file.is_file())
        .collect())
}

/// `rkat-tui --audit`: print the workspace's audit log files to standard
/// output, the rotated generation first (on standard error when there is
/// none).
///
/// # Errors
///
/// The workspace cannot be resolved, or a file cannot be read.
pub fn print(args: &crate::Args) -> Result<(), crate::TuiError> {
    let files = files(args)?;
    if files.is_empty() {
        eprintln!("no audit log for this workspace: no daemon has run for it");
        return Ok(());
    }
    for file in files {
        let text = std::fs::read_to_string(&file).map_err(|source| {
            crate::TuiError::io(format!("read the audit log {}", file.display()), source)
        })?;
        print!("{text}");
    }
    Ok(())
}

/// The gate's [`AuditSink`] in the daemon: decisions named by agent.
pub struct DaemonAudit {
    log: Arc<AuditLog>,
    agents: Weak<super::agents::Agents>,
}

impl DaemonAudit {
    /// Decisions into `log`, their agent from `agents`.
    #[must_use]
    pub fn new(log: Arc<AuditLog>, agents: &Arc<super::agents::Agents>) -> Self {
        Self {
            log,
            agents: Arc::downgrade(agents),
        }
    }

    fn agent_of(&self, session: &SessionId) -> Option<String> {
        self.agents.upgrade()?.agent_running_as(session)
    }
}

fn run_text(run: Option<&RunId>) -> Option<String> {
    run.map(ToString::to_string)
}

impl AuditSink for DaemonAudit {
    fn decision(&self, record: DecisionRecord<'_>) {
        let line = Line {
            time: now(),
            event: "decision".to_string(),
            agent: self.agent_of(record.session),
            helper: record.helper.map(str::to_string),
            session: Some(record.session.to_string()),
            run: run_text(record.run),
            tool: Some(record.tool.to_string()),
            argument: Some(record.argument),
            tier: Some(record.tier.to_string()),
            verdict: Some(if record.allowed { "allow" } else { "deny" }.to_string()),
            decider: Some(record.decider),
            reason: record.reason,
            detail: None,
            review: record.review,
        };
        self.log.write(&line);
    }

    fn event(&self, event: &str, detail: String) {
        self.log.write(&Line::event(event, detail));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_log_is_owner_only_appends_and_keeps_one_rotated_generation() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open_rotating_at(dir.path(), 200).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(log.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        for n in 0..6 {
            log.write(&Line::event("mode", format!("change {n}")).of_agent("main-abcd"));
        }
        let rotated = dir.path().join(ROTATED_FILE);
        assert!(rotated.exists(), "rotated past 200 bytes");
        let all = tail(log.path(), 100, |_| true).unwrap();
        // One generation kept: the oldest lines are gone, the newest kept
        // in order.
        assert!(all.len() < 6, "{all:?}");
        assert!(all.last().unwrap().contains("change 5"), "{all:?}");
        let last_two = tail(log.path(), 2, |_| true).unwrap();
        assert_eq!(last_two.len(), 2);
        assert!(last_two[1].contains("change 5"));
        let none = tail(log.path(), 10, |line| {
            line.agent.as_deref() == Some("other")
        })
        .unwrap();
        assert!(none.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_log_is_not_opened_through_a_symbolic_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(AUDIT_FILE)).unwrap();
        assert!(matches!(
            AuditLog::open(dir.path()),
            Err(AuditError::Open { .. })
        ));
    }
}
