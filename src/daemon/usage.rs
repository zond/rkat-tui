//! What the model calls of each conversation used, by model, counted in
//! the daemon: it sees every event of every agent and helper
//! (`super::approvals::watch`, the helper follower) and every classifier
//! call, attached client or not.
//!
//! Each conversation's [`Record`] holds its totals by model (the agent's
//! own calls, its helpers' share by label, classifier calls and compaction
//! summaries) and the context of its last model call (the client's gauge
//! seed). A schedule's runs are counted again under the schedule
//! ([`Account::Schedule`], `/schedules`). The records live in
//! `<realm>/tui_usage/`, read on first use and written behind (off the
//! async loop, at a run's end and when the daemon stops; never per
//! event). A change to a conversation's totals is pushed to the clients
//! ([`super::protocol::COST`]); a client prices and renders them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use meerkat_core::{SessionId, Usage};
use meerkat_schedule::ScheduleId;
use serde::{Deserialize, Serialize};

use crate::cost::SessionCost;
use crate::paths::{FileMode, write_atomic};

/// Directory (inside the realm root) of the usage records.
pub const USAGE_DIR: &str = "tui_usage";

/// Whose model calls a [`Record`] counts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Account {
    /// A conversation (an agent's session).
    Session(SessionId),
    /// The runs a schedule fired.
    Schedule(ScheduleId),
}

impl Account {
    /// Where its record lives in the realm at `realm_root`.
    #[must_use]
    pub fn path(&self, realm_root: &Path) -> PathBuf {
        realm_root.join(USAGE_DIR).join(self.file_name())
    }

    /// The record's file name.
    fn file_name(&self) -> String {
        match self {
            Self::Session(session) => format!("{session}.toml"),
            Self::Schedule(schedule) => format!("schedule-{schedule}.toml"),
        }
    }
}

/// The context of a conversation's last model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextReading {
    /// Tokens.
    pub tokens: u64,
    /// An estimate (a compaction's), not a provider's count.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub approx: bool,
}

/// What one account's model calls used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The context of the last model call (conversations only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextReading>,
    /// Totals by model.
    #[serde(default)]
    pub cost: SessionCost,
}

/// The daemon's usage records.
pub struct Ledger {
    realm_root: PathBuf,
    /// The records read or counted so far.
    records: parking_lot::Mutex<HashMap<Account, Record>>,
    /// Changed since they were last written.
    dirty: parking_lot::Mutex<HashSet<Account>>,
    /// One write round at a time (each writes the newest counts).
    writing: parking_lot::Mutex<()>,
    /// Tells the clients a conversation's totals changed.
    notify: Box<dyn Fn(&SessionId, &SessionCost) + Send + Sync>,
}

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger")
            .field("realm_root", &self.realm_root)
            .finish_non_exhaustive()
    }
}

impl Ledger {
    /// The records of the realm at `realm_root`; `notify` is told every
    /// change of a conversation's totals.
    #[must_use]
    pub fn new(
        realm_root: &Path,
        notify: impl Fn(&SessionId, &SessionCost) + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            realm_root: realm_root.to_path_buf(),
            records: parking_lot::Mutex::new(HashMap::new()),
            dirty: parking_lot::Mutex::new(HashSet::new()),
            writing: parking_lot::Mutex::new(()),
            notify: Box::new(notify),
        })
    }

    /// `account`'s record as counted (read from its file the first time,
    /// off the async loop).
    pub async fn get(&self, account: &Account) -> Record {
        self.load(account).await;
        self.records
            .lock()
            .get(account)
            .cloned()
            .unwrap_or_default()
    }

    /// Read `account`'s record into memory unless it is there.
    async fn load(&self, account: &Account) {
        if self.records.lock().contains_key(account) {
            return;
        }
        let path = account.path(&self.realm_root);
        let read = tokio::task::spawn_blocking(move || read_record(&path))
            .await
            .unwrap_or_default();
        self.records.lock().entry(account.clone()).or_insert(read);
    }

    /// Change `account`'s record with `change` (read first), and tell the
    /// clients when a conversation's totals changed.
    async fn change(&self, account: Account, change: impl FnOnce(&mut Record)) {
        self.load(&account).await;
        let changed = {
            let mut records = self.records.lock();
            let record = records.entry(account.clone()).or_default();
            let before = record.clone();
            change(record);
            (*record != before).then(|| record.cost.clone())
        };
        if let Some(cost) = changed {
            self.dirty.lock().insert(account.clone());
            if let Account::Session(session) = &account {
                (self.notify)(session, &cost);
            }
        }
    }

    /// One model call of the agent running as `session`, on `model`.
    pub async fn turn(&self, session: &SessionId, model: &str, usage: &Usage) {
        let tokens = crate::ui::context_tokens(usage);
        self.change(Account::Session(session.clone()), |record| {
            record.cost.record(model, usage);
            record.context = Some(ContextReading {
                tokens,
                approx: false,
            });
        })
        .await;
    }

    /// One model call of a scheduled run, counted to its schedule too.
    pub async fn scheduled(&self, schedule: &ScheduleId, model: &str, usage: &Usage) {
        self.change(Account::Schedule(schedule.clone()), |record| {
            record.cost.record(model, usage);
        })
        .await;
    }

    /// One model call of the helper `label` (on `model`) working for
    /// `owner`.
    pub async fn helper_turn(&self, owner: &SessionId, label: &str, model: &str, usage: &Usage) {
        self.change(Account::Session(owner.clone()), |record| {
            record.cost.record_helper(label, model, usage);
        })
        .await;
    }

    /// One classifier call judging a call of `session` (or of its helpers).
    pub async fn classifier(&self, session: &SessionId, model: &str, usage: &Usage) {
        self.change(Account::Session(session.clone()), |record| {
            record.cost.record(model, usage);
        })
        .await;
    }

    /// A compaction of `session` on `model`: `input` is the history
    /// estimate it summarized, `output` the summary (`summarized`: a
    /// provider call made it, which costs), `after` the estimate it left.
    pub async fn compaction(
        &self,
        session: &SessionId,
        model: &str,
        summarized: Option<(u64, u64)>,
        after: u64,
    ) {
        self.change(Account::Session(session.clone()), |record| {
            if let Some((input, output)) = summarized {
                record.cost.record_compaction(model, input, output);
            }
            record.context = Some(ContextReading {
                tokens: after,
                approx: true,
            });
        })
        .await;
    }

    /// Write what changed, off the async loop.
    pub fn persist(self: &Arc<Self>) {
        let ledger = Arc::clone(self);
        drop(tokio::task::spawn_blocking(move || ledger.write_dirty()));
    }

    /// Write what changed now (the daemon's stop; blocking file I/O).
    pub fn flush(&self) {
        self.write_dirty();
    }

    /// Write every changed record as counted now.
    fn write_dirty(&self) {
        let _one = self.writing.lock();
        let accounts: Vec<Account> = self.dirty.lock().drain().collect();
        for account in accounts {
            let Some(record) = self.records.lock().get(&account).cloned() else {
                continue;
            };
            let path = account.path(&self.realm_root);
            if let Err(err) = write_record(&path, &record) {
                tracing::warn!(error = %err, path = %path.display(), "a usage record was not written");
                // Tried again at the next write.
                self.dirty.lock().insert(account);
            }
        }
    }
}

/// A record could not be written.
#[derive(Debug, thiserror::Error)]
enum WriteError {
    #[error("cannot encode the usage record: {0}")]
    Encode(#[from] toml::ser::Error),
    #[error(transparent)]
    Write(#[from] crate::paths::AtomicWriteError),
}

fn write_record(path: &Path, record: &Record) -> Result<(), WriteError> {
    let text = toml::to_string(record)?;
    write_atomic(path, text.as_bytes(), FileMode::Private, None)?;
    Ok(())
}

/// The record at `path`; none (or an unreadable one, moved aside as
/// `<name>.corrupt-<unix seconds>` so the next write does not destroy it)
/// is an empty one.
#[must_use]
pub fn read_record(path: &Path) -> Record {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Record::default();
    };
    match toml::from_str(&text) {
        Ok(record) => record,
        Err(err) => {
            let mut aside = path.as_os_str().to_os_string();
            aside.push(format!(".corrupt-{}", crate::pricing::unix_now()));
            let moved = std::fs::rename(path, &aside);
            tracing::warn!(error = %err, path = %path.display(), moved = moved.is_ok(), "unreadable usage record moved aside");
            Record::default()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        }
    }

    /// Counts land in memory and are told; they reach the file only when
    /// written, owner-only, and a new ledger over the realm reads them.
    #[tokio::test]
    async fn counts_are_told_written_on_flush_and_read_back() {
        let realm = tempfile::tempdir().unwrap();
        let told = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let ledger = Ledger::new(realm.path(), {
            let told = Arc::clone(&told);
            move |session: &SessionId, cost: &SessionCost| {
                told.lock().push((session.clone(), cost.clone()));
            }
        });
        let session = SessionId::new();
        let schedule = ScheduleId::new();
        ledger.turn(&session, "m", &usage(10, 5)).await;
        ledger
            .helper_turn(&session, "scout", "h", &usage(3, 1))
            .await;
        ledger.classifier(&session, "judge", &usage(7, 2)).await;
        ledger.scheduled(&schedule, "m", &usage(10, 5)).await;
        // A change of nothing is not told.
        ledger
            .helper_turn(&session, "idle", "h", &usage(0, 0))
            .await;
        let path = Account::Session(session.clone()).path(realm.path());
        assert!(!path.exists(), "written only behind");
        assert_eq!(
            told.lock().len(),
            3,
            "the schedule's count is not a conversation's"
        );
        ledger.flush();
        let record = read_record(&path);
        assert_eq!(record.cost.per_model["m"].input, 10);
        assert_eq!(record.cost.per_model["h"].input, 3);
        assert_eq!(record.cost.helpers["scout"]["h"].output, 1);
        assert_eq!(record.cost.per_model["judge"].output, 2);
        assert_eq!(
            record.context.map(|c| c.tokens),
            Some(crate::ui::context_tokens(&usage(10, 5)))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let again = Ledger::new(realm.path(), |_: &SessionId, _: &SessionCost| {});
        assert_eq!(again.get(&Account::Session(session.clone())).await, record);
        let scheduled = again.get(&Account::Schedule(schedule)).await;
        assert_eq!(scheduled.cost.per_model["m"].output, 5);
        // Counting goes on from the record read.
        again.turn(&session, "m", &usage(1, 1)).await;
        again.flush();
        assert_eq!(read_record(&path).cost.per_model["m"].input, 11);
    }

    /// A compaction counts its estimate when a provider call made the
    /// summary, and leaves an estimated context either way.
    #[tokio::test]
    async fn a_compaction_counts_its_summary_call_and_estimates_the_context() {
        let realm = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(realm.path(), |_: &SessionId, _: &SessionCost| {});
        let session = SessionId::new();
        let account = Account::Session(session.clone());
        ledger.compaction(&session, "m", None, 900).await;
        let record = ledger.get(&account).await;
        assert!(record.cost.is_empty());
        assert_eq!(
            record.context,
            Some(ContextReading {
                tokens: 900,
                approx: true
            })
        );
        ledger
            .compaction(&session, "m", Some((5_000, 300)), 400)
            .await;
        let record = ledger.get(&account).await;
        assert_eq!(record.cost.per_model["m"].input, 5_000);
        assert_eq!(record.cost.per_model["m"].output, 300);
    }

    /// An unreadable record is moved aside, never overwritten, and counts
    /// start from nothing.
    #[tokio::test]
    async fn an_unreadable_record_is_moved_aside() {
        let realm = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let path = Account::Session(session.clone()).path(realm.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not = [toml").unwrap();
        let ledger = Ledger::new(realm.path(), |_: &SessionId, _: &SessionCost| {});
        ledger.turn(&session, "m", &usage(1, 1)).await;
        ledger.flush();
        assert_eq!(read_record(&path).cost.per_model["m"].input, 1);
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(names.iter().any(|n| n.contains(".corrupt-")), "{names:?}");
    }
}
