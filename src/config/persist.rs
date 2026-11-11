//! Writes to the realm's `tui.toml`.

use super::file::{AutoSection, RulesFile, UiSection};
use super::settings::ChecklistView;
use crate::approval::mode::PermissionMode;
use crate::approval::rules::{RuleOrigin, RuleSet, RulesError};
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
    // The user's rules and settings: owner-only, never through a symlink.
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
/// time, the permission mode, the checklist panel's view. The daemon owns
/// it; the approval
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
    lock: Arc<Mutex<()>>,
}

impl RealmFile {
    /// The file at `path`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Arc::new(Mutex::new(())),
        }
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
        persist_realm_rules(&self.path, &rules)
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
        persist_realm_mode(&self.path, mode)
    }

    /// Write `view` as its `[ui] checklist`.
    ///
    /// # Errors
    ///
    /// As [`Self::write_rules_with`].
    pub fn write_checklist(&self, view: ChecklistView) -> Result<(), RulesError> {
        let _one = self.lock.lock();
        persist_realm_checklist(&self.path, view)
    }
}
