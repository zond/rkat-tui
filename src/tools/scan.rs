//! What bounds the file tools' blocking work.
//!
//! Every file tool runs on the blocking pool ([`run_cancellable`]), which
//! neither a timeout nor an interrupt stops: dropping the awaiting future
//! leaves the closure running. So the closure gets a [`Cancel`] flag, set
//! when the call's future is dropped (the engine's tool timeout, an
//! interrupt, a helper's cancel) or ends, and the work checks it: a
//! search at every entry and while it reads a file, a write before it
//! commits.
//!
//! `glob` and `grep` walk the tree in parallel under a [`Scan`]: a time
//! budget and a cap on entries examined ([`SearchLimits`], `[search]` in
//! `tui.toml`). A scan that stops early returns what it found with a
//! [`Scan::partial_note`]: how much it examined, in how long, where most
//! entries were, and how to narrow it.

use ignore::{DirEntry, WalkBuilder, WalkState};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// `[search] budget_secs` when no layer sets it.
pub const DEFAULT_SEARCH_BUDGET_SECS: u64 = 10;
/// `[search] max_entries` when no layer sets it.
pub const DEFAULT_SEARCH_MAX_ENTRIES: u64 = 500_000;

/// How many of the directories holding most entries a partial answer names.
const BUSIEST_SHOWN: usize = 5;

/// How far one `glob` or `grep` may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchLimits {
    /// How long one search may take.
    pub budget: Duration,
    /// How many entries (files and directories) one search may examine.
    pub max_entries: u64,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            budget: Duration::from_secs(DEFAULT_SEARCH_BUDGET_SECS),
            max_entries: DEFAULT_SEARCH_MAX_ENTRIES,
        }
    }
}

/// Set once the call that started a piece of blocking work has gone: its
/// future was dropped or it ended.
#[derive(Debug, Clone, Default)]
pub(crate) struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Sets its flag when dropped: held across the await of the blocking work,
/// it is dropped with the call's future, whatever drops that.
struct CancelOnDrop(Cancel);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.0.store(true, Ordering::Relaxed);
    }
}

/// Run `work` on the blocking pool with a flag that is set as soon as the
/// returned future is dropped (or completes). Work still queued when the
/// call goes sees the flag set before it starts.
pub(crate) async fn run_cancellable<T, F>(work: F) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
    F: FnOnce(&Cancel) -> T + Send + 'static,
{
    let cancel = Cancel::default();
    let _guard = CancelOnDrop(cancel.clone());
    tokio::task::spawn_blocking(move || work(&cancel)).await
}

/// Why a scan stopped before it was done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    Time,
    Entries,
    Cancelled,
}

/// A test's view into the walk: every entry examined is counted, and each
/// may be slowed down so a test can drop the call mid-walk.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default)]
pub struct WalkProbe {
    /// Entries examined by every scan of the dispatcher.
    pub entries: AtomicU64,
    /// How long each entry takes.
    pub delay: Duration,
}

/// One search's budget, shared by the walk's threads.
pub(crate) struct Scan<'a> {
    root: &'a Path,
    limits: SearchLimits,
    cancel: &'a Cancel,
    started: Instant,
    examined: AtomicU64,
    stop: OnceLock<Stop>,
    /// Entries per first-level directory below the root.
    busiest: Mutex<HashMap<OsString, u64>>,
    #[cfg(any(test, feature = "test-support"))]
    probe: Option<Arc<WalkProbe>>,
}

impl<'a> Scan<'a> {
    pub(crate) fn new(root: &'a Path, limits: SearchLimits, cancel: &'a Cancel) -> Self {
        Self {
            root,
            limits,
            cancel,
            started: Instant::now(),
            examined: AtomicU64::new(0),
            stop: OnceLock::new(),
            busiest: Mutex::new(HashMap::new()),
            #[cfg(any(test, feature = "test-support"))]
            probe: None,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn with_probe(mut self, probe: Option<Arc<WalkProbe>>) -> Self {
        self.probe = probe;
        self
    }

    /// Whether the scan may go on: not cancelled, within its time, not
    /// stopped by another thread.
    pub(crate) fn go_on(&self) -> bool {
        if self.stop.get().is_some() {
            return false;
        }
        if self.cancel.is_cancelled() {
            self.halt(Stop::Cancelled);
            return false;
        }
        if self.started.elapsed() >= self.limits.budget {
            self.halt(Stop::Time);
            return false;
        }
        true
    }

    fn halt(&self, why: Stop) {
        let _ = self.stop.set(why);
    }

    /// Why the scan stopped early, if it did.
    pub(crate) fn stopped(&self) -> Option<Stop> {
        self.stop.get().copied()
    }

    /// Count one entry; `false` when it is past the cap or the scan must stop.
    fn count(&self) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(probe) = &self.probe {
            probe.entries.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(probe.delay);
        }
        if !self.go_on() {
            return false;
        }
        if self.examined.fetch_add(1, Ordering::Relaxed) >= self.limits.max_entries {
            self.halt(Stop::Entries);
            return false;
        }
        true
    }

    /// The first component of `path` below the root, when `path` lies in a
    /// subdirectory of it.
    fn first_dir<'p>(&self, path: &'p Path) -> Option<&'p OsStr> {
        let mut parts = path.strip_prefix(self.root).ok()?.components();
        let first = parts.next()?;
        parts.next()?;
        Some(first.as_os_str())
    }

    /// Walk `builder` on every thread it uses, calling `visit` for each
    /// entry with that thread's own `L`; returns every thread's `L`. A
    /// `visit` returning `false` ends the walk (the caller has enough).
    pub(crate) fn walk<L, F>(&self, builder: &WalkBuilder, visit: F) -> Vec<L>
    where
        L: Default + Send,
        F: Fn(&DirEntry, &mut L) -> bool + Sync,
    {
        let done = Mutex::new(Vec::new());
        builder.build_parallel().run(|| {
            let mut local = Local {
                scan: self,
                done: &done,
                state: Some(L::default()),
                busiest: HashMap::new(),
            };
            let visit = &visit;
            Box::new(move |entry| local.visit(entry, visit))
        });
        done.into_inner()
    }

    /// The note a stopped scan's answer ends with: how far it got, where
    /// most entries were, and `narrow` (how to narrow the search). `None`
    /// when the scan was complete.
    /// `display` shows a path the way the tool shows its results.
    pub(crate) fn partial_note(
        &self,
        narrow: &str,
        display: impl Fn(&Path) -> String,
    ) -> Option<String> {
        let why = self.stopped()?;
        let examined = self
            .examined
            .load(Ordering::Relaxed)
            .min(self.limits.max_entries);
        let secs = self.started.elapsed().as_secs_f64();
        let reason = match why {
            Stop::Time => format!(
                "it reached its time budget of {} s (`[search] budget_secs` in tui.toml)",
                self.limits.budget.as_secs_f64()
            ),
            Stop::Entries => format!(
                "it reached its cap of {} entries (`[search] max_entries` in tui.toml)",
                self.limits.max_entries
            ),
            Stop::Cancelled => "the call was cancelled".to_string(),
        };
        let mut busiest: Vec<(OsString, u64)> = self.busiest.lock().drain().collect();
        busiest.sort_by(|(da, a), (db, b)| b.cmp(a).then_with(|| da.cmp(db)));
        let busiest: Vec<String> = busiest
            .iter()
            .take(BUSIEST_SHOWN)
            .map(|(dir, count)| format!("{}/ ({count})", display(&self.root.join(dir))))
            .collect();
        let mut note = format!(
            "[partial: the search stopped after {secs:.1} s and {examined} entries examined, \
             because {reason}; there may be more matches in what it did not reach."
        );
        if !busiest.is_empty() {
            let _ = write!(
                note,
                " Most entries examined were under {}.",
                busiest.join(", ")
            );
        }
        let _ = write!(note, " To narrow it, {narrow}.]");
        Some(note)
    }
}

/// One walk thread's own state, handed back when the thread is done.
struct Local<'s, 'a, L> {
    scan: &'s Scan<'a>,
    done: &'s Mutex<Vec<L>>,
    state: Option<L>,
    busiest: HashMap<OsString, u64>,
}

impl<L> Local<'_, '_, L> {
    fn visit<F>(&mut self, entry: Result<DirEntry, ignore::Error>, visit: &F) -> WalkState
    where
        F: Fn(&DirEntry, &mut L) -> bool,
    {
        let Ok(entry) = entry else {
            return WalkState::Continue;
        };
        if !self.scan.count() {
            return WalkState::Quit;
        }
        if let Some(dir) = self.scan.first_dir(entry.path()) {
            match self.busiest.get_mut(dir) {
                Some(count) => *count += 1,
                None => {
                    self.busiest.insert(dir.to_os_string(), 1);
                }
            }
        }
        let Some(state) = self.state.as_mut() else {
            return WalkState::Quit;
        };
        if visit(&entry, state) && self.scan.go_on() {
            WalkState::Continue
        } else {
            WalkState::Quit
        }
    }
}

impl<L> Drop for Local<'_, '_, L> {
    fn drop(&mut self) {
        let mut busiest = self.scan.busiest.lock();
        for (dir, count) in self.busiest.drain() {
            *busiest.entry(dir).or_insert(0) += count;
        }
        drop(busiest);
        if let Some(state) = self.state.take() {
            self.done.lock().push(state);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_flag_is_set_when_the_call_is_dropped() {
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let call = run_cancellable(move |cancel| {
            let _ = started_tx.send(());
            let deadline = Instant::now() + Duration::from_mins(1);
            while !cancel.is_cancelled() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let _ = seen_tx.send(cancel.is_cancelled());
        });
        let call = tokio::spawn(call);
        started_rx.await.unwrap();
        call.abort();
        let seen =
            tokio::task::spawn_blocking(move || seen_rx.recv_timeout(Duration::from_secs(30)))
                .await
                .unwrap();
        assert_eq!(seen, Ok(true), "the work saw the dropped call");
    }

    #[test]
    fn a_complete_scan_has_no_partial_note() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "").unwrap();
        let cancel = Cancel::default();
        let scan = Scan::new(dir.path(), SearchLimits::default(), &cancel);
        let seen: Vec<usize> = scan.walk(&WalkBuilder::new(dir.path()), |_, n: &mut usize| {
            *n += 1;
            true
        });
        assert_eq!(seen.iter().sum::<usize>(), 2, "the root and its file");
        assert!(
            scan.partial_note("x", |p| p.display().to_string())
                .is_none()
        );
    }

    #[test]
    fn the_entry_cap_stops_a_scan_and_names_the_busiest_directories() {
        let dir = tempfile::tempdir().unwrap();
        for (sub, n) in [("big", 30), ("small", 3)] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
            for i in 0..n {
                std::fs::write(dir.path().join(sub).join(format!("f{i}")), "").unwrap();
            }
        }
        let cancel = Cancel::default();
        let limits = SearchLimits {
            budget: Duration::from_mins(10),
            max_entries: 20,
        };
        let scan = Scan::new(dir.path(), limits, &cancel);
        let seen: Vec<usize> = scan.walk(&WalkBuilder::new(dir.path()), |_, n: &mut usize| {
            *n += 1;
            true
        });
        assert!(seen.iter().sum::<usize>() <= 20);
        assert_eq!(scan.stopped(), Some(Stop::Entries));
        let note = scan
            .partial_note("pass `path`", |p| {
                p.strip_prefix(dir.path()).unwrap().display().to_string()
            })
            .unwrap();
        assert!(note.contains("under big/ ("), "{note}");
        assert!(note.contains("20 entries examined"), "{note}");
        assert!(note.contains("`[search] max_entries`"), "{note}");
        assert!(note.contains("pass `path`"), "{note}");
    }
}
