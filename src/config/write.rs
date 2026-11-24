//! The one writer of the user's and the workspace's `tui.toml`: every
//! change rkat-tui makes to either file (a setting, a rule, a statement, a
//! directory grant) goes through [`SettingsFile`].
//!
//! A change edits the file's document in place (`toml_edit`): only the
//! key it changes is touched, the comments, order and formatting of the
//! rest stay byte for byte. Before anything is written the result must
//! read as a `tui.toml` ([`RulesFile`]); the file is replaced atomically
//! ([`crate::paths::write_atomic`], which refuses a symlinked target), and
//! the read-modify-write holds an advisory lock on the file's directory
//! (`flock`), shared by every process that writes it (several daemons
//! write the user's file), beside an in-process lock. A project's
//! `.rkat/tui.toml` is never written: a [`SettingsFile`] is only ever the
//! user's or a realm's.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use toml_edit::{DocumentMut, Item, TableLike};

use super::file::{RuleEntry, RulesFile};
use super::registry::Level;

/// Why a change was not written.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// The file could not be read or locked.
    #[error("{what} {}: {source}", path.display())]
    Io {
        /// What was being done.
        what: &'static str,
        /// The file.
        path: PathBuf,
        /// The failure.
        #[source]
        source: std::io::Error,
    },
    /// The file does not parse as TOML: it is left as it is.
    #[error("{} does not parse, so it was left as it is: {source}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The parse error.
        #[source]
        source: Box<toml_edit::TomlError>,
    },
    /// The change would leave a file that does not read as `tui.toml`.
    #[error("{} was not changed: the result would not read as tui.toml: {why}", path.display())]
    Invalid {
        /// The file.
        path: PathBuf,
        /// Why.
        why: String,
    },
    /// A key on the way is not a table (`auto = 3`).
    #[error("{} was not changed: `{key}` is not a table there", path.display())]
    Shape {
        /// The file.
        path: PathBuf,
        /// The key.
        key: String,
    },
    /// The atomic replace refused or failed.
    #[error(transparent)]
    Write(#[from] crate::paths::AtomicWriteError),
}

/// The user's `~/.rkat/tui/tui.toml` or a realm's `tui.toml`, as
/// rkat-tui writes it. Clones share the in-process lock.
#[derive(Debug, Clone)]
pub struct SettingsFile {
    path: PathBuf,
    level: Level,
    lock: Arc<Mutex<()>>,
}

impl SettingsFile {
    /// The file of `level` (the user's or the workspace's) at `path`.
    #[must_use]
    pub fn new(level: Level, path: PathBuf) -> Self {
        debug_assert!(matches!(level, Level::User | Level::Workspace));
        Self {
            path,
            level,
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Where it is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read it as the layering does (missing: empty).
    ///
    /// # Errors
    ///
    /// [`crate::approval::rules::RulesError`] when it cannot be read or parsed.
    #[cfg(any(test, feature = "test-support"))]
    pub fn read(&self) -> Result<RulesFile, crate::approval::rules::RulesError> {
        RulesFile::load(&self.path)
    }

    /// Set the dotted `key` to `value` (`None`: remove it). Returns the
    /// file as it now reads.
    ///
    /// # Errors
    ///
    /// [`WriteError`]; nothing is written then.
    pub fn set(&self, key: &str, value: Option<&toml::Value>) -> Result<RulesFile, WriteError> {
        let path = self.path.clone();
        self.edit(|doc| {
            match value {
                Some(value) => {
                    let (table, last) = table_for(doc, key).ok_or_else(|| WriteError::Shape {
                        path: path.clone(),
                        key: key.to_string(),
                    })?;
                    let mut new = edit_value(value);
                    // A value replaced in place keeps its key's comments
                    // and its own spacing.
                    match table.get_mut(last).and_then(Item::as_value_mut) {
                        Some(old) => {
                            *new.decor_mut() = old.decor().clone();
                            *old = new;
                        }
                        None => {
                            table.insert(last, Item::Value(new));
                        }
                    }
                }
                None => remove_key(doc, key),
            }
            Ok(())
        })
        .map(|((), file)| file)
    }

    /// Add `item` to the list of strings at the dotted `key` (`auto.allow`,
    /// `confinement.extra_write`) unless it is there. Whether it was added.
    ///
    /// # Errors
    ///
    /// As [`Self::set`].
    pub fn add_to_list(&self, key: &str, item: &str) -> Result<bool, WriteError> {
        let path = self.path.clone();
        self.edit(|doc| {
            let shape = || WriteError::Shape {
                path: path.clone(),
                key: key.to_string(),
            };
            let (table, last) = table_for(doc, key).ok_or_else(shape)?;
            let list = table
                .entry(last)
                .or_insert(Item::Value(
                    toml_edit::Value::Array(toml_edit::Array::new()),
                ))
                .as_array_mut()
                .ok_or_else(shape)?;
            if list.iter().any(|known| known.as_str() == Some(item)) {
                return Ok(false);
            }
            list.push(item);
            Ok(true)
        })
        .map(|(added, _)| added)
    }

    /// Remove `item` from the list of strings at the dotted `key`. Whether
    /// it was there.
    ///
    /// # Errors
    ///
    /// As [`Self::set`].
    pub fn remove_from_list(&self, key: &str, item: &str) -> Result<bool, WriteError> {
        self.remove_matching(key, |known| known == item)
    }

    /// Remove every string of the list at the dotted `key` that `matches`
    /// (a trusted host however it was spelled). Whether one was there.
    ///
    /// # Errors
    ///
    /// As [`Self::set`].
    pub fn remove_matching(
        &self,
        key: &str,
        matches: impl Fn(&str) -> bool,
    ) -> Result<bool, WriteError> {
        self.edit(|doc| {
            let Some(list) = item_at(doc, key).and_then(Item::as_array_mut) else {
                return Ok(false);
            };
            let before = list.len();
            // The first entry's spacing stays the list's first, whichever
            // entry that is now.
            let lead = list.get(0).map(|first| first.decor().prefix().cloned());
            list.retain(|known| !known.as_str().is_some_and(&matches));
            if let (Some(lead), Some(first)) = (lead, list.get_mut(0)) {
                first.decor_mut().set_prefix(lead.unwrap_or_default());
            }
            Ok(list.len() != before)
        })
        .map(|(removed, _)| removed)
    }

    /// Add `entry` to the `[[allow]]` or `[[deny]]` list (`list`) unless an
    /// equal one is there. Whether it was added.
    ///
    /// # Errors
    ///
    /// As [`Self::set`].
    pub fn add_rule(&self, list: &str, entry: &RuleEntry) -> Result<bool, WriteError> {
        let path = self.path.clone();
        self.edit(|doc| {
            let shape = || WriteError::Shape {
                path: path.clone(),
                key: list.to_string(),
            };
            let item = doc
                .as_table_mut()
                .entry(list)
                .or_insert(Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
            if rule_entries(item).iter().any(|known| known == entry) {
                return Ok(false);
            }
            push_rule(item, entry).ok_or_else(shape)?;
            Ok(true)
        })
        .map(|(added, _)| added)
    }

    /// Replace the `[[allow]]` or `[[deny]]` entry equal to `old` (in
    /// `old_list`) with `new` (of `new_list`), in one write. Within one
    /// list the entry changes where it stands, its comments and layout
    /// kept (a field's value keeps its own comment); into the other list
    /// it leaves its place and joins that list's end. Whether `old` was
    /// there (nothing is written when it was not).
    ///
    /// # Errors
    ///
    /// As [`Self::set`], and when a list is not an array of tables.
    pub fn replace_rule(
        &self,
        old_list: &str,
        old: &RuleEntry,
        new_list: &str,
        new: &RuleEntry,
    ) -> Result<bool, WriteError> {
        let path = self.path.clone();
        self.edit(|doc| {
            let shape = |key: &str| WriteError::Shape {
                path: path.clone(),
                key: key.to_string(),
            };
            let Some(item) = doc.as_table_mut().get_mut(old_list) else {
                return Ok(false);
            };
            let Some(at) = rule_entries(item).iter().position(|known| known == old) else {
                return Ok(false);
            };
            if old_list == new_list {
                let table: &mut dyn TableLike = match item {
                    Item::ArrayOfTables(tables) => {
                        tables.get_mut(at).ok_or_else(|| shape(old_list))?
                    }
                    Item::Value(toml_edit::Value::Array(array)) => array
                        .get_mut(at)
                        .and_then(toml_edit::Value::as_inline_table_mut)
                        .ok_or_else(|| shape(old_list))?,
                    _ => return Err(shape(old_list)),
                };
                set_field(table, "tool", Some(&new.tool));
                set_field(table, "arg", new.arg.as_deref());
                set_field(table, "literal", new.literal.as_deref());
                return Ok(true);
            }
            match item {
                Item::ArrayOfTables(tables) => {
                    tables.remove(at);
                }
                Item::Value(toml_edit::Value::Array(array)) => {
                    array.remove(at);
                }
                _ => return Err(shape(old_list)),
            }
            let item = doc
                .as_table_mut()
                .entry(new_list)
                .or_insert(Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
            push_rule(item, new).ok_or_else(|| shape(new_list))?;
            Ok(true)
        })
        .map(|(replaced, _)| replaced)
    }

    /// Remove the `[[allow]]` or `[[deny]]` entry equal to `entry`.
    /// Whether one was there.
    ///
    /// # Errors
    ///
    /// As [`Self::set`].
    pub fn remove_rule(&self, list: &str, entry: &RuleEntry) -> Result<bool, WriteError> {
        self.edit(|doc| {
            let Some(item) = doc.as_table_mut().get_mut(list) else {
                return Ok(false);
            };
            let Some(at) = rule_entries(item).iter().position(|known| known == entry) else {
                return Ok(false);
            };
            match item {
                Item::ArrayOfTables(tables) => {
                    tables.remove(at);
                }
                Item::Value(toml_edit::Value::Array(array)) => {
                    array.remove(at);
                }
                _ => return Ok(false),
            }
            Ok(true)
        })
        .map(|(removed, _)| removed)
    }

    /// Read the file under both locks, apply `change` to its document,
    /// check the result reads as `tui.toml`, and replace the file when it
    /// changed. Returns `change`'s answer and the file as it now reads.
    fn edit<R>(
        &self,
        change: impl FnOnce(&mut DocumentMut) -> Result<R, WriteError>,
    ) -> Result<(R, RulesFile), WriteError> {
        let _here = self.lock.lock();
        let _everywhere = self.lock_directory()?;
        let old = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(WriteError::Io {
                    what: "read",
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let mut doc: DocumentMut = old.parse().map_err(|source| WriteError::Parse {
            path: self.path.clone(),
            source: Box::new(source),
        })?;
        let answer = change(&mut doc)?;
        let new = doc.to_string();
        let file: RulesFile = toml::from_str(&new).map_err(|err| WriteError::Invalid {
            path: self.path.clone(),
            why: err.message().to_string(),
        })?;
        if new != old {
            let (mode, trusted) = match self.level {
                // The realm's: owner-only, as it always was written.
                Level::Workspace => (
                    crate::paths::FileMode::Private,
                    self.path.parent().and_then(Path::parent),
                ),
                // The user's own file keeps the mode the user gave it.
                _ => (crate::paths::FileMode::Preserve, self.path.parent()),
            };
            crate::paths::write_atomic(&self.path, new.as_bytes(), mode, trusted)?;
        }
        Ok((answer, file))
    }

    /// An exclusive advisory lock on the file's directory, released when
    /// the handle drops (the directory is created when missing).
    fn lock_directory(&self) -> Result<std::fs::File, WriteError> {
        let io = |what, source| WriteError::Io {
            what,
            path: self.path.clone(),
            source,
        };
        let dir = self
            .path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        crate::paths::check_no_symlinks(dir)?;
        std::fs::create_dir_all(dir).map_err(|err| io("create the directory of", err))?;
        let handle = std::fs::File::open(dir).map_err(|err| io("open the directory of", err))?;
        handle
            .lock()
            .map_err(|err| io("lock the directory of", err))?;
        Ok(handle)
    }
}

/// The table that holds the dotted `key`'s last part, created on the way
/// (`[auto]` for `auto.mode`), and that part; `None` when a part on the
/// way is not a table.
fn table_for<'d, 'k>(
    doc: &'d mut DocumentMut,
    key: &'k str,
) -> Option<(&'d mut dyn TableLike, &'k str)> {
    let (parents, last) = match key.rsplit_once('.') {
        Some((parents, last)) => (Some(parents), last),
        None => (None, key),
    };
    let mut table: &mut dyn TableLike = doc.as_table_mut();
    for part in parents.into_iter().flat_map(|parents| parents.split('.')) {
        table = table
            .entry(part)
            .or_insert_with(toml_edit::table)
            .as_table_like_mut()?;
    }
    Some((table, last))
}

/// The item at the dotted `key`, when it is there.
fn item_at<'d>(doc: &'d mut DocumentMut, key: &str) -> Option<&'d mut Item> {
    let mut parts = key.split('.');
    let mut item = doc.as_table_mut().get_mut(parts.next()?)?;
    for part in parts {
        item = item.as_table_like_mut()?.get_mut(part)?;
    }
    Some(item)
}

/// Remove the dotted `key`, when it is there.
fn remove_key(doc: &mut DocumentMut, key: &str) {
    let (parents, last) = match key.rsplit_once('.') {
        Some((parents, last)) => (Some(parents), last),
        None => (None, key),
    };
    let table = match parents {
        Some(parents) => item_at(doc, parents).and_then(Item::as_table_like_mut),
        None => Some(doc.as_table_mut() as &mut dyn TableLike),
    };
    if let Some(table) = table {
        table.remove(last);
    }
}

/// `value` as a document value (TOML's own spelling of it).
fn edit_value(value: &toml::Value) -> toml_edit::Value {
    value
        .to_string()
        .parse()
        .unwrap_or_else(|_| toml_edit::Value::from(value.to_string()))
}

/// Append `entry` to the `allow`/`deny` item (an array of tables, or an
/// inline array); `None` when the item is neither.
fn push_rule(item: &mut Item, entry: &RuleEntry) -> Option<()> {
    let mut fields = vec![("tool", entry.tool.as_str())];
    fields.extend(entry.arg.as_deref().map(|arg| ("arg", arg)));
    fields.extend(entry.literal.as_deref().map(|literal| ("literal", literal)));
    match item {
        Item::ArrayOfTables(tables) => {
            let mut table = toml_edit::Table::new();
            for (name, value) in fields {
                table.insert(name, toml_edit::value(value));
            }
            tables.push(table);
        }
        Item::Value(toml_edit::Value::Array(array)) => {
            let mut table = toml_edit::InlineTable::new();
            for (name, value) in fields {
                table.insert(name, value.into());
            }
            array.push(table);
        }
        _ => return None,
    }
    Some(())
}

/// Set a rule entry's field `name` to `value` where it stands (its value's
/// own comment kept), add it at the end when it is not there, or remove it
/// (`None`).
fn set_field(table: &mut dyn TableLike, name: &str, value: Option<&str>) {
    let Some(text) = value else {
        table.remove(name);
        return;
    };
    match table.get_mut(name).and_then(Item::as_value_mut) {
        Some(held) => {
            let decor = held.decor().clone();
            *held = toml_edit::Value::from(text);
            *held.decor_mut() = decor;
        }
        None => {
            table.insert(name, toml_edit::value(text));
        }
    }
}

/// The rule entries of an `allow`/`deny` item, in order.
fn rule_entries(item: &Item) -> Vec<RuleEntry> {
    let field = |table: &dyn TableLike, name: &str| {
        table.get(name).and_then(Item::as_str).map(str::to_string)
    };
    let entry = |table: &dyn TableLike| RuleEntry {
        tool: field(table, "tool").unwrap_or_default(),
        arg: field(table, "arg"),
        literal: field(table, "literal"),
    };
    match item {
        Item::ArrayOfTables(tables) => tables.iter().map(|table| entry(table)).collect(),
        Item::Value(toml_edit::Value::Array(array)) => array
            .iter()
            .filter_map(toml_edit::Value::as_inline_table)
            .map(|table| entry(table))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;
