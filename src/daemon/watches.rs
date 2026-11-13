//! The agents' file watches: what `watch_paths` registers.
//!
//! The daemon's process holds one recursive file-system watch on the
//! workspace (the `notify` crate, set up on the first watch and dropped
//! when the last watch goes), never the agent's confined shell. Every
//! changed path is matched against each agent's watches; a path no watch
//! matches is dropped at once (it neither opens a batch nor delays one).
//! The matches of one agent are batched until no matching change came for
//! [`DEBOUNCE`], or the batch is [`MAX_BATCH_AGE`] old (a build that writes
//! continuously does not hold it back), and leave as one [`Batch`] (at
//! most [`MAX_PATHS`] paths named, then "and N more"; at most
//! [`MAX_TRACKED`] are remembered per agent and batch, the rest only
//! counted), which the daemon delivers to the agent as a message into its
//! session (`super::agents::Agents::watch_event`).
//!
//! A watch made with `once` is dropped after its first batch; any other
//! lives until the run that made it ends ([`Watches::end_turn`]), the
//! agent's `unwatch`, or its retirement ([`Watches::forget`]). A change the
//! agent's own file tools made (`write_file`, `edit_file`, which report a
//! diff) is not reported back to it ([`Watches::own_write`]); a change its
//! shell made is, since nothing tells the two apart. A file created and
//! gone within one batch (a write's temporary file, renamed over its
//! target) is not reported to anyone.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use globset::{Glob, GlobSet, GlobSetBuilder};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::tools::WatchHost;

/// How long a batch waits for more changes.
pub const DEBOUNCE: Duration = Duration::from_millis(250);
/// The oldest a batch gets before it leaves, changes or not.
pub const MAX_BATCH_AGE: Duration = Duration::from_secs(2);
/// How many changed paths one agent's batch remembers; more are counted.
pub const MAX_TRACKED: usize = 4096;
/// How many paths one batch names.
pub const MAX_PATHS: usize = 50;
/// How close to its own write's mark a change of the same file is the
/// agent's (measured from when the change was seen, not when its batch
/// leaves).
const OWN_WRITE_WINDOW: Duration = Duration::from_secs(2);

/// What happened to a path, as far as transient files are concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// It was created.
    Created,
    /// It was renamed away or removed.
    Gone,
    /// Anything else (written, renamed to, metadata).
    Other,
}

/// One agent's changes, ready to deliver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// The agent.
    pub agent: String,
    /// The changed paths (relative to the workspace), at most
    /// [`MAX_PATHS`].
    pub paths: Vec<String>,
    /// How many more changed.
    pub more: usize,
}

impl Batch {
    /// The message the agent gets: `files changed: a, b and 3 more`.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = format!("files changed: {}", self.paths.join(", "));
        if self.more > 0 {
            text.push_str(&format!(" and {} more", self.more));
        }
        text
    }
}

struct Watch {
    id: u64,
    patterns: Vec<String>,
    set: GlobSet,
    once: bool,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    watches: BTreeMap<String, Vec<Watch>>,
    /// Matched paths per agent, waiting for the batch (at most
    /// [`MAX_TRACKED`] each).
    pending: BTreeMap<String, BTreeSet<String>>,
    /// Matched paths past [`MAX_TRACKED`] per agent: only counted.
    overflow: BTreeMap<String, usize>,
    /// The paths created in the open batch, and those gone since: one
    /// that is both was transient (a write's temporary file renamed over
    /// its target) and is not reported.
    created: BTreeSet<String>,
    gone: BTreeSet<String>,
    /// The `once` watches that matched, per agent: dropped at the flush.
    fired_once: BTreeMap<String, BTreeSet<u64>>,
    /// When each agent's file tools last wrote each path (marked before
    /// the write and again after it).
    own: HashMap<(String, String), Instant>,
    /// When each path matched in the open batch was last seen to change
    /// (at most [`MAX_TRACKED`]): an own write is judged against when its
    /// change happened, not when the batch leaves.
    observed: HashMap<String, Instant>,
}

/// The agents' watches on one workspace.
pub struct Watches {
    root: PathBuf,
    inner: parking_lot::Mutex<Inner>,
    /// The changed paths, from the file-system watch to the batcher.
    changes: mpsc::UnboundedSender<(PathBuf, Change)>,
    watcher: parking_lot::Mutex<Option<notify::RecommendedWatcher>>,
}

impl std::fmt::Debug for Watches {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watches")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Watches {
    /// The watches of the workspace at `root`, and where their batches
    /// arrive. Spawns the batcher, which ends when the watches are dropped.
    #[must_use]
    pub fn new(root: PathBuf) -> (Arc<Self>, mpsc::UnboundedReceiver<Batch>) {
        let (changes, changed) = mpsc::unbounded_channel();
        let (batches, batched) = mpsc::unbounded_channel();
        let watches = Arc::new(Self {
            root,
            inner: parking_lot::Mutex::new(Inner::default()),
            changes,
            watcher: parking_lot::Mutex::new(None),
        });
        tokio::spawn(batch(Arc::downgrade(&watches), changed, batches));
        (watches, batched)
    }

    /// A changed path: queued for each watch it matches (the file-system
    /// watch sends its own; tests feed paths here).
    #[cfg(any(test, feature = "test-support"))]
    pub fn observe(&self, path: &Path, change: Change) {
        let _ = self.changes.send((path.to_path_buf(), change));
    }

    /// `agent`'s file tools wrote `path` (absolute or relative to the
    /// workspace): a change of it soon after is not reported to it.
    pub fn own_write(&self, agent: &str, path: &Path) {
        let Some(relative) = self.relative(path) else {
            return;
        };
        self.inner
            .lock()
            .own
            .insert((agent.to_string(), relative), Instant::now());
    }

    /// `agent`'s run ended: its watches other than `once` ones go.
    pub fn end_turn(&self, agent: &str) {
        {
            let mut inner = self.inner.lock();
            if let Some(list) = inner.watches.get_mut(agent) {
                list.retain(|watch| watch.once);
            }
        }
        self.release_if_idle();
    }

    /// `agent` retired: every watch of its goes, and what was pending.
    pub fn forget(&self, agent: &str) {
        {
            let mut inner = self.inner.lock();
            inner.watches.remove(agent);
            inner.pending.remove(agent);
            inner.overflow.remove(agent);
            inner.fired_once.remove(agent);
            inner.own.retain(|(owner, _), _| owner != agent);
        }
        self.release_if_idle();
    }

    /// Drop the file-system watch when no watch is left (taken with the
    /// watcher lock first, as [`WatchHost::watch`] does, so a watch being
    /// added keeps it).
    fn release_if_idle(&self) {
        let mut slot = self.watcher.lock();
        if slot.is_some() && self.inner.lock().watches.values().all(Vec::is_empty) {
            *slot = None;
            tracing::debug!("no file watch left: the workspace is not watched any more");
        }
    }

    /// Whether the workspace is watched now (tests).
    #[cfg(test)]
    fn watching(&self) -> bool {
        self.watcher.lock().is_some()
    }

    /// `agent`'s live watches, as `/status` shows them.
    #[must_use]
    pub fn describe(&self, agent: &str) -> Vec<String> {
        self.inner
            .lock()
            .watches
            .get(agent)
            .map(|list| {
                list.iter()
                    .map(|watch| {
                        format!(
                            "watch {}: {}{}",
                            watch.id,
                            watch.patterns.join(", "),
                            if watch.once { " (once)" } else { "" }
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `path` relative to the workspace, when it is inside it.
    fn relative(&self, path: &Path) -> Option<String> {
        let relative = if path.is_absolute() {
            path.strip_prefix(&self.root).ok()?
        } else {
            path
        };
        let text = relative.to_str()?.to_string();
        (!text.is_empty()).then_some(text)
    }

    /// Queue `path` for the watches it matches; whether any did (a path
    /// nobody watches leaves no trace).
    fn queue(&self, path: &Path, change: Change) -> bool {
        let Some(relative) = self.relative(path) else {
            return false;
        };
        let mut inner = self.inner.lock();
        let Inner {
            watches,
            pending,
            overflow,
            fired_once,
            created,
            gone,
            observed,
            ..
        } = &mut *inner;
        let mut matched = false;
        for (agent, list) in watches.iter() {
            for watch in list {
                if !watch.set.is_match(&relative) {
                    continue;
                }
                matched = true;
                let paths = pending.entry(agent.clone()).or_default();
                if paths.len() < MAX_TRACKED || paths.contains(&relative) {
                    paths.insert(relative.clone());
                } else {
                    *overflow.entry(agent.clone()).or_default() += 1;
                }
                if watch.once {
                    fired_once
                        .entry(agent.clone())
                        .or_default()
                        .insert(watch.id);
                }
            }
        }
        if matched {
            if observed.len() < MAX_TRACKED || observed.contains_key(&relative) {
                observed.insert(relative.clone(), Instant::now());
            }
            let transient = match change {
                Change::Created => Some(&mut *created),
                Change::Gone => Some(&mut *gone),
                Change::Other => None,
            };
            if let Some(set) = transient
                && set.len() < MAX_TRACKED
            {
                set.insert(relative);
            }
        }
        matched
    }

    /// The batches due now: each agent's pending paths less its own
    /// writes; the `once` watches that matched are dropped.
    fn flush(&self) -> Vec<Batch> {
        let batches = self.take_batches();
        self.release_if_idle();
        batches
    }

    fn take_batches(&self) -> Vec<Batch> {
        let mut inner = self.inner.lock();
        let now = Instant::now();
        let pending = std::mem::take(&mut inner.pending);
        let observed = std::mem::take(&mut inner.observed);
        // An agent's change: one seen within the window of its own mark,
        // however late the batch leaves.
        let own = |inner: &Inner, agent: &str, path: &str| {
            let Some(at) = inner.own.get(&(agent.to_string(), path.to_string())) else {
                return false;
            };
            let seen = observed.get(path).copied().unwrap_or(now);
            seen.saturating_duration_since(*at)
                .max(at.saturating_duration_since(seen))
                < OWN_WRITE_WINDOW
        };
        let mut overflow = std::mem::take(&mut inner.overflow);
        let created = std::mem::take(&mut inner.created);
        let gone = std::mem::take(&mut inner.gone);
        let fired = std::mem::take(&mut inner.fired_once);
        for (agent, ids) in &fired {
            if let Some(list) = inner.watches.get_mut(agent) {
                list.retain(|watch| !ids.contains(&watch.id));
            }
        }
        let mut batches = Vec::new();
        for (agent, paths) in pending {
            let paths: Vec<String> = paths
                .into_iter()
                .filter(|path| !own(&inner, &agent, path))
                .filter(|path| !(created.contains(path) && gone.contains(path)))
                .collect();
            let counted = overflow.remove(&agent).unwrap_or(0);
            if paths.is_empty() && counted == 0 {
                continue;
            }
            let more = paths.len().saturating_sub(MAX_PATHS) + counted;
            batches.push(Batch {
                agent,
                paths: paths.into_iter().take(MAX_PATHS).collect(),
                more,
            });
        }
        // Judged: what is past the window now cannot cover a later change.
        inner
            .own
            .retain(|_, at| now.saturating_duration_since(*at) < OWN_WRITE_WINDOW);
        batches
    }

    /// The file-system watch on the workspace in `slot` (the watcher
    /// lock), set up when there is none.
    fn ensure_watcher(&self, slot: &mut Option<notify::RecommendedWatcher>) -> Result<(), String> {
        use notify::Watcher as _;
        if slot.is_some() {
            return Ok(());
        }
        let changes = self.changes.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else {
                    return;
                };
                use notify::event::{ModifyKind, RenameMode};
                let change = match event.kind {
                    notify::EventKind::Access(_) => return,
                    notify::EventKind::Create(_) => Change::Created,
                    notify::EventKind::Remove(_)
                    | notify::EventKind::Modify(ModifyKind::Name(RenameMode::From)) => Change::Gone,
                    _ => Change::Other,
                };
                for path in event.paths {
                    let _ = changes.send((path, change));
                }
            })
            .map_err(|err| format!("cannot watch the workspace: {err}"))?;
        watcher
            .watch(&self.root, notify::RecursiveMode::Recursive)
            .map_err(|err| format!("cannot watch the workspace: {err}"))?;
        *slot = Some(watcher);
        Ok(())
    }
}

impl WatchHost for Watches {
    fn watch(&self, agent: &str, patterns: &[String], once: bool) -> Result<u64, String> {
        if patterns.is_empty() {
            return Err(
                "name at least one pattern in `patterns`, for example [\"src/**/*.rs\"]"
                    .to_string(),
            );
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern.trim_start_matches("./")).map_err(|err| {
                format!(
                    "invalid pattern `{pattern}`: {err}. Patterns are relative to the project \
                         root and use `*`, `**`, `?`, `[abc]` and `{{a,b}}`, for example \
                         `src/**/*.rs`"
                )
            })?;
            builder.add(glob);
        }
        let set = builder
            .build()
            .map_err(|err| format!("invalid patterns: {err}"))?;
        // The watcher lock is held until the watch is in the table, so the
        // last watch going elsewhere cannot drop the watcher meanwhile.
        let mut slot = self.watcher.lock();
        self.ensure_watcher(&mut slot)?;
        let mut inner = self.inner.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        inner
            .watches
            .entry(agent.to_string())
            .or_default()
            .push(Watch {
                id,
                patterns: patterns.to_vec(),
                set,
                once,
            });
        Ok(id)
    }

    fn unwatch(&self, agent: &str, id: Option<u64>) -> usize {
        let removed = {
            let mut inner = self.inner.lock();
            let Some(list) = inner.watches.get_mut(agent) else {
                return 0;
            };
            let before = list.len();
            match id {
                Some(id) => list.retain(|watch| watch.id != id),
                None => list.clear(),
            }
            before - list.len()
        };
        self.release_if_idle();
        removed
    }
}

/// Batch the changes: the first matching change after a quiet spell opens
/// a batch, which is flushed once no matching change came for [`DEBOUNCE`]
/// or it is [`MAX_BATCH_AGE`] old. A change no watch matches is dropped.
async fn batch(
    watches: std::sync::Weak<Watches>,
    mut changed: mpsc::UnboundedReceiver<(PathBuf, Change)>,
    batches: mpsc::UnboundedSender<Batch>,
) {
    while let Some((first, change)) = changed.recv().await {
        let Some(live) = watches.upgrade() else {
            return;
        };
        if !live.queue(&first, change) {
            continue;
        }
        let opened = Instant::now();
        let mut last = opened;
        loop {
            let due = (last + DEBOUNCE).min(opened + MAX_BATCH_AGE);
            match tokio::time::timeout_at(due, changed.recv()).await {
                Ok(Some((path, change))) => {
                    if live.queue(&path, change) {
                        last = Instant::now();
                    }
                }
                Ok(None) => return,
                Err(_) => break,
            }
        }
        for batch in live.flush() {
            if batches.send(batch).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn watches() -> (Arc<Watches>, mpsc::UnboundedReceiver<Batch>) {
        Watches::new(PathBuf::from("/w"))
    }

    /// Without the file-system watch: a watch registered directly.
    fn add(w: &Watches, agent: &str, patterns: &[&str], once: bool) -> u64 {
        let mut builder = GlobSetBuilder::new();
        for p in patterns {
            builder.add(Glob::new(p).unwrap());
        }
        let mut inner = w.inner.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.watches.entry(agent.into()).or_default().push(Watch {
            id,
            patterns: patterns.iter().map(ToString::to_string).collect(),
            set: builder.build().unwrap(),
            once,
        });
        id
    }

    #[tokio::test(start_paused = true)]
    async fn changes_are_batched_matched_and_capped() {
        let (w, mut rx) = watches();
        add(&w, "main-a", &["src/**"], false);
        add(&w, "main-b", &["*.toml"], false);
        for n in 0..(MAX_PATHS + 3) {
            w.observe(Path::new(&format!("/w/src/f{n:02}.rs")), Change::Other);
        }
        w.observe(Path::new("/w/Cargo.toml"), Change::Other);
        w.observe(Path::new("/elsewhere/src/x.rs"), Change::Other);
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        assert_eq!(first.agent, "main-a");
        assert_eq!(first.paths.len(), MAX_PATHS);
        assert_eq!(first.more, 3);
        assert!(first.text().ends_with(" and 3 more"), "{}", first.text());
        assert_eq!(
            second,
            Batch {
                agent: "main-b".into(),
                paths: vec!["Cargo.toml".into()],
                more: 0
            }
        );
    }

    /// Changes no watch matches neither delay a batch nor are kept: a
    /// matching change is delivered after the debounce although unmatched
    /// ones keep coming, and the transient-file sets stay empty.
    #[tokio::test(start_paused = true)]
    async fn unmatched_changes_do_not_hold_a_batch_or_grow_the_state() {
        let (w, mut rx) = watches();
        add(&w, "main-a", &["src/**"], false);
        let feeder = {
            let w = Arc::clone(&w);
            tokio::spawn(async move {
                for n in 0..100 {
                    w.observe(Path::new(&format!("/w/target/o{n}")), Change::Created);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        let started = Instant::now();
        w.observe(Path::new("/w/src/lib.rs"), Change::Other);
        let batch = rx.recv().await.unwrap();
        assert_eq!(batch.paths, ["src/lib.rs"]);
        assert!(started.elapsed() < DEBOUNCE * 2, "{:?}", started.elapsed());
        {
            let inner = w.inner.lock();
            assert!(inner.created.is_empty() && inner.gone.is_empty());
            assert!(inner.pending.is_empty());
        }
        feeder.abort();
    }

    /// A watched path that keeps changing (a build) does not hold its
    /// batch back past [`MAX_BATCH_AGE`].
    #[tokio::test(start_paused = true)]
    async fn a_continuously_changing_tree_is_delivered_within_the_batch_age() {
        let (w, mut rx) = watches();
        add(&w, "main-a", &["src/**"], false);
        let feeder = {
            let w = Arc::clone(&w);
            tokio::spawn(async move {
                for n in 0..1000 {
                    w.observe(Path::new(&format!("/w/src/gen{}.rs", n % 7)), Change::Other);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
        };
        let started = Instant::now();
        let batch = rx.recv().await.unwrap();
        let waited = started.elapsed();
        assert!(
            waited >= MAX_BATCH_AGE && waited < MAX_BATCH_AGE + DEBOUNCE,
            "{waited:?}"
        );
        assert_eq!(batch.paths.len(), 7);
        feeder.abort();
    }

    /// One batch remembers at most [`MAX_TRACKED`] paths per agent and
    /// counts the rest.
    #[tokio::test(start_paused = true)]
    async fn a_batch_counts_what_it_does_not_remember() {
        let (w, mut rx) = watches();
        add(&w, "main-a", &["src/**"], false);
        for n in 0..(MAX_TRACKED + 5) {
            w.observe(Path::new(&format!("/w/src/f{n}.rs")), Change::Other);
        }
        let batch = rx.recv().await.unwrap();
        assert_eq!(batch.paths.len(), MAX_PATHS);
        assert_eq!(batch.more, MAX_TRACKED + 5 - MAX_PATHS);
    }

    /// A watch the agent asked for wrongly is refused with what to pass
    /// instead.
    #[tokio::test]
    async fn a_refused_watch_says_what_to_pass() {
        let dir = tempfile::tempdir().unwrap();
        let (w, _rx) = Watches::new(dir.path().to_path_buf());
        let none = w.watch("main-a", &[], false).unwrap_err();
        assert!(none.contains(r#"for example ["src/**/*.rs"]"#), "{none}");
        let bad = w
            .watch("main-a", &["src/[a".to_string()], false)
            .unwrap_err();
        assert!(bad.contains("for example `src/**/*.rs`"), "{bad}");
    }

    /// The recursive file-system watch goes when the last watch goes: by
    /// `unwatch`, at the turn's end, with the retired agent, and after a
    /// `once` watch fired.
    #[tokio::test]
    async fn the_workspace_watch_is_dropped_with_the_last_watch() {
        let dir = tempfile::tempdir().unwrap();
        let (w, mut rx) = Watches::new(dir.path().to_path_buf());
        let patterns = vec!["*.txt".to_string()];
        assert!(!w.watching());
        let id = w.watch("main-a", &patterns, false).unwrap();
        let other = w.watch("main-a", &patterns, false).unwrap();
        assert!(w.watching());
        assert_eq!(w.unwatch("main-a", Some(id)), 1);
        assert!(w.watching(), "one watch is left");
        assert_eq!(w.unwatch("main-a", Some(other)), 1);
        assert!(!w.watching());
        w.watch("main-a", &patterns, false).unwrap();
        w.end_turn("main-a");
        assert!(!w.watching());
        w.watch("main-b", &patterns, false).unwrap();
        w.forget("main-b");
        assert!(!w.watching());
        w.watch("main-a", &patterns, true).unwrap();
        w.observe(&dir.path().join("a.txt"), Change::Other);
        assert_eq!(rx.recv().await.unwrap().paths, ["a.txt"]);
        assert!(!w.watching(), "the once watch fired and went");
    }

    #[tokio::test(start_paused = true)]
    async fn once_goes_after_its_event_and_the_others_with_the_turn() {
        let (w, mut rx) = watches();
        add(&w, "main-a", &["a.txt"], true);
        add(&w, "main-a", &["b.txt"], false);
        assert_eq!(w.describe("main-a").len(), 2);
        w.observe(Path::new("/w/a.txt"), Change::Other);
        assert_eq!(rx.recv().await.unwrap().paths, ["a.txt"]);
        assert_eq!(w.describe("main-a"), ["watch 2: b.txt"]);
        w.end_turn("main-a");
        assert!(w.describe("main-a").is_empty());
        // A once watch outlives the turn; retiring drops it.
        add(&w, "main-a", &["c.txt"], true);
        w.end_turn("main-a");
        assert_eq!(w.describe("main-a"), ["watch 3: c.txt (once)"]);
        w.forget("main-a");
        assert!(w.describe("main-a").is_empty());
        assert_eq!(w.unwatch("main-a", None), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_agent_s_own_writes_are_not_reported_to_it() {
        let (w, mut rx) = watches();
        add(&w, "main-a", &["src/**"], false);
        w.own_write("main-a", Path::new("src/own.rs"));
        // The write's temporary file, renamed over the target.
        w.observe(Path::new("/w/src/.tmpAb12"), Change::Created);
        w.observe(Path::new("/w/src/.tmpAb12"), Change::Gone);
        w.observe(Path::new("/w/src/own.rs"), Change::Other);
        w.observe(Path::new("/w/src/theirs.rs"), Change::Other);
        // A deleted file is a change.
        w.observe(Path::new("/w/src/old.rs"), Change::Gone);
        assert_eq!(
            rx.recv().await.unwrap().paths,
            ["src/old.rs", "src/theirs.rs"]
        );
        // Another agent's watch still hears it.
        add(&w, "main-b", &["src/**"], false);
        w.own_write("main-a", Path::new("/w/src/own.rs"));
        w.observe(Path::new("/w/src/own.rs"), Change::Other);
        let batch = rx.recv().await.unwrap();
        assert_eq!(
            (batch.agent.as_str(), batch.paths.as_slice()),
            ("main-b", &["src/own.rs".to_string()][..])
        );
    }

    /// A batch that leaves late (a starved runtime) still knows the
    /// agent's own write: it is judged by when the change was seen, which
    /// was right after the mark; a change seen well after the window is
    /// someone else's.
    #[tokio::test(start_paused = true)]
    async fn an_own_write_flushed_late_is_still_the_agent_s() {
        let (w, _rx) = watches();
        add(&w, "main-a", &["src/**"], false);
        w.own_write("main-a", Path::new("src/own.rs"));
        // Queued directly: the batcher does not flush it meanwhile.
        assert!(w.queue(Path::new("/w/src/own.rs"), Change::Other));
        tokio::time::advance(OWN_WRITE_WINDOW * 2).await;
        assert_eq!(w.take_batches(), Vec::new());
        // The mark is gone with that flush: a later change is reported.
        assert!(w.queue(Path::new("/w/src/own.rs"), Change::Other));
        assert_eq!(
            w.take_batches(),
            vec![Batch {
                agent: "main-a".into(),
                paths: vec!["src/own.rs".into()],
                more: 0
            }]
        );
        // A mark long before the change does not cover it.
        w.own_write("main-a", Path::new("src/own.rs"));
        tokio::time::advance(OWN_WRITE_WINDOW * 2).await;
        assert!(w.queue(Path::new("/w/src/own.rs"), Change::Other));
        assert_eq!(w.take_batches().len(), 1);
    }
}
