//! The daemon's settings: every `tui.toml` layer's values as they are now
//! ([`Settings`]), the one path a change takes, and the rows `/settings`
//! shows.
//!
//! A change (`/set`, `/unset`, a shorthand such as `/compact prepare`, a
//! mode switch) is checked by its setting ([`Setting::parse`]), written to
//! the user's or the workspace's file through the one writer
//! ([`crate::config::write::SettingsFile`]), and the layered value
//! recomputed from every layer ([`Setting::resolve`]); when the value in
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
//! already and finds nothing to do.

use std::path::PathBuf;

use parking_lot::{Mutex, RwLock};
use toml::Value;

use super::protocol::{SectionRow, SettingRow, SettingsList};
use crate::config::registry::{
    Applies, Layers, Level, ProjectRule, Resolved, SECTIONS, SETTINGS, Setting, put, sections_of,
    show, with_scalars,
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
}

/// What reading a settings file again found.
#[derive(Debug, Clone)]
pub enum Synced {
    /// It holds what the daemon holds (its own write, or a touch).
    Same,
    /// It changed: each setting whose value in force moved, and whether a
    /// structured section (rules, statements, grants, endpoints, prices,
    /// thinking) changed.
    Changed {
        /// The settings in force that moved.
        changes: Vec<Change>,
        /// A section changed (it applies at the next start).
        sections: bool,
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
        Self {
            files,
            project,
            layers: RwLock::new(layers),
            changing: Mutex::new(()),
            unreadable: Mutex::default(),
        }
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
        let drop: Vec<&str> = setting.alias.iter().map(|alias| alias.key).collect();
        let _one = self.changing.lock();
        let before = self.resolve(setting);
        file.set(setting.key, value.as_ref(), &drop)
            .map_err(|err| format!("{} was not changed: {err}", setting.key))?;
        // Only the key written: a change someone else made to the file
        // meanwhile is the next sync's to find and apply.
        if let Some(table) = self.layers.write().of_mut(level) {
            for old in &drop {
                put(table, old, None);
            }
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
        let Some(path) = self.path_of(level) else {
            return;
        };
        let _one = self.changing.lock();
        match RulesFile::load(&path) {
            Ok(read) => {
                if let Some(table) = self.layers.write().of_mut(level) {
                    *table = with_scalars(&read.table(), table);
                }
            }
            Err(err) => tracing::warn!(%err, "a settings file could not be read again"),
        }
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
        self.unreadable.lock().remove(&level);
        let table = read.table();
        let mut layers = self.layers.write();
        let before = layers.clone();
        let Some(held) = layers.of_mut(level) else {
            return Synced::Same;
        };
        let sections = sections_of(held) != sections_of(&table);
        *held = table;
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
        if changes.is_empty() && !sections {
            Synced::Same
        } else {
            Synced::Changed { changes, sections }
        }
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
