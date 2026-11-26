//! The daemon's settings: every `tui.toml` layer's values as they are now
//! ([`Settings`]), the one path a change takes, and the rows `/settings`
//! shows.
//!
//! A change (`/set`, `/unset`, a shorthand such as `/compact prepare`, a
//! mode switch) is checked by its setting ([`Setting::parse`]), written to
//! the user's or the workspace's file through the one writer
//! ([`crate::config::write::SettingsFile`]), and the layered value
//! recomputed from every layer ([`Setting::resolve`]). When the value in
//! force changed, the local methods run the setting's hook
//! (`super::local`'s `apply_setting`: the mode, the classifier model,
//! `trust_project_builds`, the prepare instruction and threshold, the
//! `[ui]` settings the clients follow). [`Written::confirmation`] says
//! what was written where, what is in force and from which layer, when it
//! takes effect, and which higher layer wins over it.
//!
//! The files are followed while the daemon runs (`super::settings_watch`):
//! [`Settings::sync`] reads one again and diffs it against what the daemon
//! holds, by content: the daemon's own write holds what the file holds
//! already and finds nothing to do. The permission sections the user's
//! own layers hold are what [`Settings::lasting`] computes, as at start.

use std::path::PathBuf;

use parking_lot::{Mutex, RwLock};
use toml::Value;

use super::protocol::{SectionRow, SettingRow, SettingsList};
use crate::config::registry::{
    Applies, KeyPath, Layers, Level, ProjectRule, Resolved, SECTIONS, SETTINGS, Setting,
    other_sections_of, path_words, paths_overlap, permissions_of, put, sections_of, show, value_at,
    with_scalars, without_replaced,
};
use crate::config::{RulesFile, SettingsFiles};

/// Every layer's values and the files the user's and the workspace's come
/// from.
#[derive(Debug)]
pub struct Settings {
    files: Option<SettingsFiles>,
    /// The project's `.rkat/tui.toml` (read, never written).
    project: Option<PathBuf>,
    layers: RwLock<Layers>,
    /// Held across a write and the layers' update, so two changes cannot
    /// update the layers in the other order than they wrote the file.
    changing: Mutex<()>,
    /// The error each file that does not read was last reported with.
    unreadable: Mutex<std::collections::HashMap<Level, String>>,
    /// What each file last held at the keys given with `--replace` (which
    /// the layers never hold), to tell a change of them once.
    pinned_seen: Mutex<std::collections::HashMap<Level, Vec<(KeyPath, Value)>>>,
}

/// What reading a settings file again found.
#[derive(Debug, Clone)]
pub enum Synced {
    /// It holds what the daemon holds (its own write, or a touch).
    Same,
    /// It changed: each setting whose value in force moved, and which
    /// structured sections changed.
    Changed {
        /// The settings in force that moved.
        changes: Vec<Change>,
        /// The permission sections (rules, statements, named directories)
        /// of the user's or the workspace's file changed: they apply at
        /// once ([`Settings::lasting`]).
        permissions: bool,
        /// Another section changed (endpoints, prices, thinking; or any
        /// section of the project's file): it applies at the next start.
        later: bool,
        /// Keys the daemon's command line gives with `--replace` whose
        /// value in the file changed: not taken (as written on the command
        /// line).
        pinned: Vec<String>,
    },
    /// It does not read, for the first time with this error: its last
    /// good values stay in force.
    Unreadable(String),
    /// It still does not read, as reported.
    StillUnreadable,
}

/// One setting whose value in force moved.
#[derive(Debug, Clone)]
pub struct Change {
    /// The setting.
    pub setting: &'static Setting,
    /// In force before.
    pub before: Resolved,
    /// In force now.
    pub after: Resolved,
}

/// One change, written: the setting, the level and file, the value given,
/// and what was in force before and is after.
#[derive(Debug, Clone)]
pub struct Written {
    /// The setting.
    pub setting: &'static Setting,
    /// The level written.
    pub level: Level,
    /// The file written.
    pub path: PathBuf,
    /// The value written (`None`: removed).
    pub value: Option<Value>,
    /// In force before.
    pub before: Resolved,
    /// In force now.
    pub after: Resolved,
}

impl Settings {
    /// The settings over `layers` (as read at start), written to `files`,
    /// with the project's file at `project`.
    #[must_use]
    pub fn new(files: Option<SettingsFiles>, layers: Layers, project: Option<PathBuf>) -> Self {
        // What the files hold at the `--replace` keys now: the start told
        // it (a notice per key), so a sync tells only a change of it.
        let mut seen = std::collections::HashMap::new();
        if let Some(files) = files.as_ref().filter(|_| !layers.replaced.is_empty()) {
            for level in [Level::User, Level::Workspace] {
                if let Ok(read) = RulesFile::load(files.of(level).path()) {
                    let raw = read.table();
                    let held: Vec<(KeyPath, Value)> = layers
                        .replaced
                        .iter()
                        .filter_map(|path| Some((path.clone(), value_at(&raw, path)?.clone())))
                        .collect();
                    seen.insert(level, held);
                }
            }
        }
        Self {
            files,
            project,
            layers: RwLock::new(layers),
            changing: Mutex::new(()),
            unreadable: Mutex::default(),
            pinned_seen: Mutex::new(seen),
        }
    }

    /// The `--replace` key of the daemon's command line that `key` falls
    /// under or holds, as written there.
    #[must_use]
    pub fn pinned(&self, key: &str) -> Option<String> {
        let path: KeyPath = key.split('.').map(str::to_string).collect();
        self.layers
            .read()
            .replaced
            .iter()
            .find(|replaced| paths_overlap(replaced, &path))
            .map(|replaced| path_words(replaced))
    }

    /// `table` (a user's or a workspace's file as read) without the keys
    /// given with `--replace`.
    fn unpinned(&self, table: &toml::Table) -> toml::Table {
        let replaced = self.layers.read().replaced.clone();
        without_replaced(table, &replaced).0
    }

    /// The file of `level` (the user's, the workspace's or the project's).
    #[must_use]
    pub fn path_of(&self, level: Level) -> Option<PathBuf> {
        match level {
            Level::User | Level::Workspace => self
                .files
                .as_ref()
                .map(|files| files.of(level).path().to_path_buf()),
            Level::Project => self.project.clone(),
            Level::Default | Level::CommandLine => None,
        }
    }

    /// Whether the daemon has the user's and the workspace's files.
    #[must_use]
    pub fn has_files(&self) -> bool {
        self.files.is_some()
    }

    /// The files followed while the daemon runs, each with its level.
    #[must_use]
    pub fn watched(&self) -> Vec<(Level, PathBuf)> {
        [Level::User, Level::Workspace, Level::Project]
            .into_iter()
            .filter_map(|level| Some((level, self.path_of(level)?)))
            .collect()
    }

    /// `setting`'s value in force now.
    #[must_use]
    pub fn resolve(&self, setting: &Setting) -> Resolved {
        setting.resolve(&self.layers.read())
    }

    /// `setting`'s value in force without `level`'s.
    #[must_use]
    pub fn resolve_without(&self, setting: &Setting, level: Level) -> Resolved {
        setting.resolve_without(&self.layers.read(), Some(level))
    }

    /// Every layer's values now.
    #[must_use]
    pub fn layers(&self) -> Layers {
        self.layers.read().clone()
    }

    /// Write `text` (`None`: remove the value) as `key`'s value of the
    /// user's file (`user`) or the workspace's, and recompute it. Blocking
    /// file I/O.
    ///
    /// # Errors
    ///
    /// Why nothing was written, for the user: an unknown key (a section's
    /// names its commands), a value the setting does not take, no file, or
    /// the writer's refusal.
    pub fn write(&self, key: &str, text: Option<&str>, user: bool) -> Result<Written, String> {
        let setting = find(key)?;
        let value = text.map(|text| setting.parse(text)).transpose()?;
        self.write_value(
            setting,
            value,
            if user { Level::User } else { Level::Workspace },
        )
    }

    /// Write `value` as `setting`'s value at `level` (the user's or the
    /// workspace's), and recompute it.
    ///
    /// # Errors
    ///
    /// As [`Self::write`].
    pub fn write_value(
        &self,
        setting: &'static Setting,
        value: Option<Value>,
        level: Level,
    ) -> Result<Written, String> {
        let files = self
            .files
            .as_ref()
            .ok_or_else(|| "this daemon has no tui.toml to write".to_string())?;
        let file = files.of(level);
        if let Some(replaced) = self.pinned(setting.key) {
            return Err(format!(
                "{} was not changed: the daemon's command line gives {replaced} with --replace, which holds it until the daemon restarts; restart without that --replace to change it",
                setting.key
            ));
        }
        let _one = self.changing.lock();
        let before = self.resolve(setting);
        let mut candidate = self.layers();
        if let Some(table) = candidate.of_mut(level) {
            put(table, setting.key, value.clone());
        }
        if let Err(err) = crate::config::helper_models::HelperModels::of(&candidate) {
            return Err(format!("{} was not changed: {err}", setting.key));
        }
        file.set(setting.key, value.as_ref())
            .map_err(|err| format!("{} was not changed: {err}", setting.key))?;
        // Only the key written: a change someone else made to the file
        // meanwhile is the next sync's to find and apply.
        if let Some(table) = self.layers.write().of_mut(level) {
            put(table, setting.key, value.clone());
        }
        let after = self.resolve(setting);
        Ok(Written {
            setting,
            level,
            path: file.path().to_path_buf(),
            value,
            before,
            after,
        })
    }

    /// A rule, grant or statement was written to `level`'s file: hold its
    /// sections as the file has them now (its scalar settings stay as
    /// held: a change of one there is the next sync's).
    pub fn adopt_sections(&self, level: Level) {
        let _one = self.changing.lock();
        self.adopt_held(level);
    }

    /// [`Self::adopt_sections`], the caller holding `changing`.
    fn adopt_held(&self, level: Level) {
        let Some(path) = self.path_of(level) else {
            return;
        };
        match RulesFile::load(&path) {
            Ok(read) => {
                let read = self.unpinned(&read.table());
                if let Some(table) = self.layers.write().of_mut(level) {
                    *table = with_scalars(&read, table);
                }
            }
            Err(err) => tracing::warn!(%err, "a settings file could not be read again"),
        }
    }

    /// The files held still: no write, sync or adoption of them until the
    /// guard drops (the hold of what they give,
    /// `super::local::Local::hold_permissions`, reads them and changes the
    /// engine under it, so no change of the engine's and its file's can
    /// land between the two).
    pub fn still(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.changing.lock()
    }

    /// Whether the files are held still now (a write, sync or hold is under
    /// way): tests make a sync arrive while they are.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn held_still(&self) -> bool {
        self.changing.is_locked()
    }

    /// Read `level`'s file again and hold what it says now: the settings
    /// whose value in force moved, whether a section changed, or that it
    /// does not read (reported once per error; the last good values stay).
    /// Blocking file I/O.
    pub fn sync(&self, level: Level) -> Synced {
        let Some(path) = self.path_of(level) else {
            return Synced::Same;
        };
        let _one = self.changing.lock();
        let read = match RulesFile::load(&path) {
            Ok(read) => read,
            Err(err) => {
                let why = err.to_string();
                let mut unreadable = self.unreadable.lock();
                if unreadable.get(&level) == Some(&why) {
                    return Synced::StillUnreadable;
                }
                unreadable.insert(level, why.clone());
                return Synced::Unreadable(why);
            }
        };
        let raw = read.table();
        // The keys given with `--replace` stay the command line's: a file's
        // value of one is never taken, and a change of it is told once.
        let (table, pinned) = if level == Level::Project {
            (raw, Vec::new())
        } else {
            let replaced = self.layers.read().replaced.clone();
            let now: Vec<(KeyPath, Value)> = replaced
                .iter()
                .filter_map(|path| Some((path.clone(), value_at(&raw, path)?.clone())))
                .collect();
            let mut seen = self.pinned_seen.lock();
            let before = seen.get(&level).cloned().unwrap_or_default();
            let pinned: Vec<String> = now
                .iter()
                .filter(|entry| !before.contains(entry))
                .map(|(path, _)| path_words(path))
                .collect();
            seen.insert(level, now);
            (without_replaced(&raw, &replaced).0, pinned)
        };
        let mut layers = self.layers.write();
        let before = layers.clone();
        let mut after = before.clone();
        let Some(held) = after.of_mut(level) else {
            return Synced::Same;
        };
        let moved = |of: fn(&toml::Table) -> toml::Table| of(held) != of(&table);
        let (permissions, later) = if level == Level::Project {
            // A project's sections are taken at start only.
            (false, moved(sections_of))
        } else {
            (moved(permissions_of), moved(other_sections_of))
        };
        *held = table;
        // Settings that only make sense together (the helper model lists)
        // are checked over every layer: a file that breaks them is not
        // applied, as one that does not read.
        if let Err(err) = crate::config::helper_models::HelperModels::of(&after) {
            let why = err.to_string();
            let mut unreadable = self.unreadable.lock();
            if unreadable.get(&level) == Some(&why) {
                return Synced::StillUnreadable;
            }
            unreadable.insert(level, why.clone());
            return Synced::Unreadable(why);
        }
        self.unreadable.lock().remove(&level);
        *layers = after;
        let changes: Vec<Change> = SETTINGS
            .iter()
            .filter_map(|setting| {
                let (before, after) = (setting.resolve(&before), setting.resolve(&layers));
                (before != after).then_some(Change {
                    setting,
                    before,
                    after,
                })
            })
            .collect();
        if changes.is_empty() && !permissions && !later && pinned.is_empty() {
            Synced::Same
        } else {
            Synced::Changed {
                changes,
                permissions,
                later,
                pinned,
            }
        }
    }

    /// The lasting permissions the user's own layers give now (the user's
    /// file, the workspace's and the command line's, as held): the one
    /// computation the start made ([`crate::config::lasting::Lasting::of`]).
    #[must_use]
    pub fn lasting(&self) -> crate::config::lasting::Lasting {
        use crate::approval::rules::RuleOrigin;
        let layers = self.layers.read();
        let read = |table: &toml::Table| -> RulesFile {
            // Each table was read as a `RulesFile` and is one.
            Value::Table(table.clone())
                .try_into()
                .unwrap_or_else(|err| {
                    tracing::warn!(%err, "a settings layer as held does not read");
                    RulesFile::default()
                })
        };
        let (user, workspace, cli) = (
            read(&layers.user),
            read(&layers.workspace),
            read(&layers.cli),
        );
        crate::config::lasting::Lasting::of(&[
            (RuleOrigin::Global, &user),
            (RuleOrigin::Realm, &workspace),
            (RuleOrigin::Cli, &cli),
        ])
    }

    /// Every setting as `/settings` lists it.
    #[must_use]
    pub fn rows(&self) -> SettingsList {
        let layers = self.layers.read().clone();
        let rows = SETTINGS
            .iter()
            .map(|setting| {
                let resolved = setting.resolve(&layers);
                SettingRow {
                    key: setting.key.to_string(),
                    about: setting.about.to_string(),
                    takes: setting.kind.describe(),
                    applies: setting.applies,
                    value: in_force_words(setting, &resolved),
                    source: resolved.source,
                    levels: Level::LAYERS
                        .into_iter()
                        .filter_map(|level| Some((level, show(&setting.at(&layers, level)?))))
                        .collect(),
                    default: setting.default.describe(),
                    project: setting.project.describe().to_string(),
                    command: setting.command.map(str::to_string),
                }
            })
            .collect();
        let sections = SECTIONS
            .iter()
            .map(|section| SectionRow {
                key: section.key.to_string(),
                about: section.about.to_string(),
                project: section.project.to_string(),
                applies: section.applies.to_string(),
                flags: section.flags.to_string(),
                commands: section.commands.to_string(),
            })
            .collect();
        let path = |level| {
            self.files
                .as_ref()
                .map(|files: &SettingsFiles| files.of(level).path().to_path_buf())
                .unwrap_or_default()
        };
        SettingsList {
            rows,
            sections,
            user_file: path(Level::User),
            workspace_file: path(Level::Workspace),
        }
    }
}

/// The approval engine's changes of the files' permission sections go in
/// under `changing`, as the commands' writes and the syncs do, and end
/// with both files' sections held as written: a sync after one finds
/// nothing to do.
impl crate::approval::engine::PermissionFiles for Settings {
    fn writing(&self) -> Box<dyn crate::approval::engine::Writing + '_> {
        Box::new(EngineWriting {
            settings: self,
            _one: self.changing.lock(),
        })
    }
}

/// [`Settings`]' guard for one of the engine's changes.
struct EngineWriting<'a> {
    settings: &'a Settings,
    _one: parking_lot::MutexGuard<'a, ()>,
}

impl crate::approval::engine::Writing for EngineWriting<'_> {}

impl Drop for EngineWriting<'_> {
    fn drop(&mut self) {
        // Before the lock goes with the fields.
        self.settings.adopt_held(Level::User);
        self.settings.adopt_held(Level::Workspace);
    }
}

/// The setting `key` names.
///
/// # Errors
///
/// An unknown key: a section's says which commands change it, any other
/// names the keys that start alike.
pub fn find(key: &str) -> Result<&'static Setting, String> {
    let key = key.trim();
    if let Some(setting) = Setting::find(key) {
        return Ok(setting);
    }
    let head = key.split('.').next().unwrap_or(key);
    if let Some(section) = SECTIONS
        .iter()
        .find(|section| section.key.trim_start_matches("[[").starts_with(head))
    {
        return Err(format!(
            "{key} is part of the structured section {}: change it with {} or in the file",
            section.key, section.commands
        ));
    }
    let alike: Vec<&str> = SETTINGS
        .iter()
        .map(|setting| setting.key)
        .filter(|known| known.starts_with(head) || known.contains(key))
        .collect();
    Err(if alike.is_empty() {
        format!("there is no setting {key} (/settings lists them)")
    } else {
        format!(
            "there is no setting {key}; did you mean {}? (/settings lists them)",
            alike.join(", ")
        )
    })
}

/// The value in force in words (the default's description when unset).
#[must_use]
pub fn in_force_words(setting: &Setting, resolved: &Resolved) -> String {
    match &resolved.value {
        Some(Value::String(text)) if text.is_empty() => "\"\" (off)".to_string(),
        Some(value) => show(value),
        None => setting.default.describe(),
    }
}

impl Written {
    /// Whether the value in force changed.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.before.value != self.after.value
    }

    /// The level written, in words, with its file.
    fn file_words(&self) -> String {
        format!("{} ({})", self.level.file_words(), self.path.display())
    }

    /// What was written where, what is in force and from where, when it
    /// takes effect, and which higher layer wins over it.
    #[must_use]
    pub fn confirmation(&self) -> String {
        let key = self.setting.key;
        let mut parts = vec![match &self.value {
            Some(value) => format!("{key} = {}: written to {}", show(value), self.file_words()),
            None => format!("{key}: removed from {}", self.file_words()),
        }];
        parts.push(format!(
            "in force: {} ({})",
            in_force_words(self.setting, &self.after),
            self.after.source.source_words()
        ));
        parts.push(
            match (self.changed(), self.setting.applies) {
                (false, _) => "the value in force did not change",
                (true, Applies::Live) => "applied at once",
                (true, Applies::NextConversation) => "it applies from the next conversation",
                (true, Applies::NextStart) => "it applies at the next start",
            }
            .to_string(),
        );
        if self.after.source > self.level {
            let held = in_force_words(self.setting, &self.after);
            parts.push(match self.after.source {
                Level::CommandLine => format!(
                    "the daemon's command line sets it ({held}), which wins until the daemon restarts"
                ),
                Level::Project if self.setting.project == ProjectRule::OnlyLower => format!(
                    "the project's .rkat/tui.toml lowers it to {held}, which wins"
                ),
                Level::Project => {
                    format!("the project's .rkat/tui.toml sets it ({held}), which wins")
                }
                _ => format!(
                    "this workspace's tui.toml sets it ({held}), which wins here: /unset {key} removes it"
                ),
            });
        }
        parts.join("; ")
    }
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
