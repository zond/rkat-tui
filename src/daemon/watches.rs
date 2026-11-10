//! The agents' file watches (P6 item B): what `watch_paths` registers.
//!
//! The daemon's process holds one recursive file-system watch on the
//! workspace (the `notify` crate, set up on the first watch), never the
//! agent's confined shell. Every changed path is matched against each
//! agent's watches; the matches of one agent are batched until no change
//! came for [`DEBOUNCE`] and leave as one [`Batch`] (at most [`MAX_PATHS`]
//! paths named, then "and N more"), which the daemon delivers to the agent
//! as a message into its session (`super::agents::Agents::watch_event`).
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
/// How many paths one batch names.
pub const MAX_PATHS: usize = 50;
/// How long after its own write a change of the same file is the agent's.
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
    /// Matched paths per agent, waiting for the batch.
    pending: BTreeMap<String, BTreeSet<String>>,
    /// The paths created in the open batch, and those gone since: one
    /// that is both was transient (a write's temporary file renamed over
    /// its target) and is not reported.
    created: BTreeSet<String>,
    gone: BTreeSet<String>,
    /// The `once` watches that matched, per agent: dropped at the flush.
    fired_once: BTreeMap<String, BTreeSet<u64>>,
    /// When each agent's file tools last wrote each path.
    own: HashMap<(String, String), Instant>,
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
        let mut inner = self.inner.lock();
        if let Some(list) = inner.watches.get_mut(agent) {
            list.retain(|watch| watch.once);
        }
    }

    /// `agent` retired: every watch of its goes, and what was pending.
    pub fn forget(&self, agent: &str) {
        let mut inner = self.inner.lock();
        inner.watches.remove(agent);
        inner.pending.remove(agent);
        inner.fired_once.remove(agent);
        inner.own.retain(|(owner, _), _| owner != agent);
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

    /// Queue `path` for the watches it matches.
    fn queue(&self, path: &Path, change: Change) {
        let Some(relative) = self.relative(path) else {
            return;
        };
        let mut inner = self.inner.lock();
        match change {
            Change::Created => {
                inner.created.insert(relative.clone());
            }
            Change::Gone => {
                inner.gone.insert(relative.clone());
            }
            Change::Other => {}
        }
        let Inner {
            watches,
            pending,
            fired_once,
            ..
        } = &mut *inner;
        for (agent, list) in watches.iter() {
            for watch in list {
                if watch.set.is_match(&relative) {
                    pending
                        .entry(agent.clone())
                        .or_default()
                        .insert(relative.clone());
                    if watch.once {
                        fired_once
                            .entry(agent.clone())
                            .or_default()
                            .insert(watch.id);
                    }
                }
            }
        }
    }

    /// The batches due now: each agent's pending paths less its own
    /// writes; the `once` watches that matched are dropped.
    fn flush(&self) -> Vec<Batch> {
        let mut inner = self.inner.lock();
        let now = Instant::now();
        inner
            .own
            .retain(|_, at| now.saturating_duration_since(*at) < OWN_WRITE_WINDOW);
        let pending = std::mem::take(&mut inner.pending);
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
                .filter(|path| !inner.own.contains_key(&(agent.clone(), path.clone())))
                .filter(|path| !(created.contains(path) && gone.contains(path)))
                .collect();
            if paths.is_empty() {
                continue;
            }
            let more = paths.len().saturating_sub(MAX_PATHS);
            batches.push(Batch {
                agent,
                paths: paths.into_iter().take(MAX_PATHS).collect(),
                more,
            });
        }
        batches
    }

    /// The file-system watch on the workspace, set up on first use.
    fn ensure_watcher(&self) -> Result<(), String> {
        use notify::Watcher as _;
        let mut slot = self.watcher.lock();
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
            return Err("name at least one pattern".to_string());
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern.trim_start_matches("./"))
                .map_err(|err| format!("invalid pattern `{pattern}`: {err}"))?;
            builder.add(glob);
        }
        let set = builder
            .build()
            .map_err(|err| format!("invalid patterns: {err}"))?;
        self.ensure_watcher()?;
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
    }
}

/// Batch the changes: the first change after a quiet spell opens a batch,
/// which is flushed once no change came for [`DEBOUNCE`].
async fn batch(
    watches: std::sync::Weak<Watches>,
    mut changed: mpsc::UnboundedReceiver<(PathBuf, Change)>,
    batches: mpsc::UnboundedSender<Batch>,
) {
    while let Some((first, change)) = changed.recv().await {
        let Some(live) = watches.upgrade() else {
            return;
        };
        live.queue(&first, change);
        loop {
            match tokio::time::timeout(DEBOUNCE, changed.recv()).await {
                Ok(Some((path, change))) => live.queue(&path, change),
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
}
