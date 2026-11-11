//! Writes to the realm's `tui.toml`.

use super::file::{AutoSection, RuleEntry, RulesFile, UiSection};
use super::settings::ChecklistView;
use crate::approval::mode::PermissionMode;
use crate::approval::rules::{RuleOrigin, RuleSet, RulesError};
use crate::bootstrap::trust::HostTrustRecord;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Atomically write a rules file (temp file in the same directory + rename).
///
/// # Errors
///
/// Returns [`RulesError`] on serialization or I/O failures.
pub fn save_realm(path: &Path, file: &RulesFile) -> Result<(), RulesError> {
    let text = toml::to_string_pretty(file)?;
    // Realm files can hold endpoint keys: owner-only, never through a symlink.
    let trusted = path.parent().and_then(Path::parent);
    crate::paths::write_atomic(
        path,
        text.as_bytes(),
        crate::paths::FileMode::Private,
        trusted,
    )
    .map_err(|source| RulesError::Write {
        path: path.to_path_buf(),
        source,
    })
}

/// A human-readable summary of a rules file (what `/permissions trust` shows
/// before trusting it).
#[must_use]
pub fn summarize(path: &Path, file: &RulesFile) -> String {
    use std::fmt::Write as _;
    let mut out = format!("{}:", path.display());
    let describe = |entry: &RuleEntry| match (&entry.arg, &entry.literal) {
        (Some(arg), _) => format!("{} {arg}", entry.tool),
        (None, Some(literal)) => format!("{} ={literal:?}", entry.tool),
        (None, None) => entry.tool.clone(),
    };
    for entry in &file.allow {
        let _ = write!(out, "\n  allow {}", describe(entry));
    }
    for entry in &file.deny {
        let _ = write!(out, "\n  deny {}", describe(entry));
    }
    if file
        .auto
        .as_ref()
        .is_some_and(|a| *a != AutoSection::default())
    {
        out.push_str("\n  [auto] settings");
    }
    for name in file.endpoints.keys() {
        let _ = write!(out, "\n  endpoint {name}");
    }
    if !file.pricing.is_empty() {
        let _ = write!(out, "\n  {} price override(s)", file.pricing.len());
    }
    if !file.thinking.is_empty() {
        let _ = write!(out, "\n  {} thinking setting(s)", file.thinking.len());
    }
    out
}

/// Rewrite the rule lists of the realm file from the realm/session rules in
/// `rules`, preserving its `[auto]` section.
///
/// # Errors
///
/// Returns [`RulesError`] on I/O, parse or serialization failures.
pub fn persist_realm_rules(path: &Path, rules: &RuleSet) -> Result<(), RulesError> {
    let mut file = RulesFile::load(path)?;
    let (mut allow, mut deny) = rules.layer_entries(RuleOrigin::Realm);
    let (session_allow, session_deny) = rules.layer_entries(RuleOrigin::Session);
    allow.extend(session_allow);
    deny.extend(session_deny);
    file.allow = allow;
    file.deny = deny;
    save_realm(path, &file)
}

/// Record `mode` as the realm's `[auto] mode` (the older `enabled` key is
/// dropped: `mode` says it), keeping everything else in the file.
///
/// # Errors
///
/// Returns [`RulesError`] when the file cannot be read, parsed or written.
pub fn persist_realm_mode(path: &Path, mode: PermissionMode) -> Result<(), RulesError> {
    let mut file = RulesFile::load(path)?;
    let auto = file.auto.get_or_insert_with(AutoSection::default);
    auto.enabled = None;
    auto.mode = Some(mode);
    save_realm(path, &file)
}

/// Record `view` as the realm's `[ui] checklist`, keeping everything else
/// in the file.
///
/// # Errors
///
/// Returns [`RulesError`] when the file cannot be read, parsed or written.
pub fn persist_realm_checklist(path: &Path, view: ChecklistView) -> Result<(), RulesError> {
    let mut file = RulesFile::load(path)?;
    file.ui.get_or_insert_with(UiSection::default).checklist = Some(view);
    save_realm(path, &file)
}

/// The realm's `tui.toml` as rkat-tui writes it: the rules added at run
/// time, the permission mode, the checklist panel's view. Every write is
/// recorded in the host trust record (when there is one), so the next
/// start trusts what rkat-tui wrote. The daemon owns it; the approval
/// engine is handed it for the rules and the mode, and `rkat/checklist_view`
/// writes through it.
///
/// Every write is a read-modify-write of the whole file under one lock
/// shared by every clone, and what it writes is read under that lock
/// ([`Self::write_rules_with`], [`Self::write_mode_with`]), so two writers
/// (a rule added while the mode changes) cannot write in the other order
/// and drop one's change from the file.
#[derive(Debug, Clone)]
pub struct RealmFile {
    path: PathBuf,
    trust: Option<Arc<Mutex<HostTrustRecord>>>,
    lock: Arc<Mutex<()>>,
}

impl RealmFile {
    /// The file at `path`, its writes recorded in `trust`.
    #[must_use]
    pub fn new(path: PathBuf, trust: Option<Arc<Mutex<HostTrustRecord>>>) -> Self {
        Self {
            path,
            trust,
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Where it is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The host trust record its writes are recorded in.
    #[must_use]
    pub fn trust(&self) -> Option<&Arc<Mutex<HostTrustRecord>>> {
        self.trust.as_ref()
    }

    /// Write the rules `rules` gives as the file's rules, called under the
    /// file's lock (so the last writer writes the newest rules).
    ///
    /// # Errors
    ///
    /// [`RulesError`] when the file cannot be read, parsed or written.
    pub fn write_rules_with(&self, rules: impl FnOnce() -> RuleSet) -> Result<(), RulesError> {
        let _one = self.lock.lock();
        let rules = rules();
        write_trusted(&self.path, self.trust.as_deref(), |path| {
            persist_realm_rules(path, &rules)
        })
    }

    /// Write the mode `mode` gives as its `[auto] mode`, called under the
    /// file's lock.
    ///
    /// # Errors
    ///
    /// As [`Self::write_rules_with`].
    pub fn write_mode_with(&self, mode: impl FnOnce() -> PermissionMode) -> Result<(), RulesError> {
        let _one = self.lock.lock();
        let mode = mode();
        write_trusted(&self.path, self.trust.as_deref(), |path| {
            persist_realm_mode(path, mode)
        })
    }

    /// Write `view` as its `[ui] checklist`.
    ///
    /// # Errors
    ///
    /// As [`Self::write_rules_with`].
    pub fn write_checklist(&self, view: ChecklistView) -> Result<(), RulesError> {
        let _one = self.lock.lock();
        write_trusted(&self.path, self.trust.as_deref(), |path| {
            persist_realm_checklist(path, view)
        })
    }
}

/// Run `write` on the realm file at `path`, then record what was written
/// in the host trust record `trust`, so the next start applies all of it
/// (see [`crate::bootstrap::trust`]). A failure to record is logged: the
/// write stands, and the next start applies only its deny rules.
///
/// # Errors
///
/// Returns what `write` returns.
pub fn write_trusted(
    path: &Path,
    trust: Option<&Mutex<HostTrustRecord>>,
    write: impl FnOnce(&Path) -> Result<(), RulesError>,
) -> Result<(), RulesError> {
    write(path)?;
    if let Some(trust) = trust
        && let Err(err) = trust.lock().record_file(path)
    {
        tracing::warn!(error = %err, "realm rules written but not recorded as trusted");
    }
    Ok(())
}
