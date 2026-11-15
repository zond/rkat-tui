//! Following the settings files while the daemon runs: the user's own
//! `tui.toml`, the workspace's realm `tui.toml` and the project's
//! `.rkat/tui.toml` are watched (the `notify` crate, each file's directory,
//! or its nearest existing ancestor until it appears, without recursion;
//! only events naming the file count), and a change is read again after a
//! short quiet time ([`DEBOUNCE`], at most [`MAX_WAIT`]) and synced
//! ([`super::local::Local::sync_settings`]): each setting whose value in
//! force changed runs its hook, the clients get one notice per setting,
//! and an audit line records it. The daemon's own writes find the file as
//! it holds it already and change nothing (compared by content, not by
//! timing); a file that does not parse is reported once and its last good
//! values stay in force.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Weak;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::config::registry::Level;

/// How long the files must be quiet before they are read again (an editor
/// writes in steps; two daemons' writes come close together).
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// The longest a change waits for that quiet: a file touched more often
/// than every [`DEBOUNCE`] is still read this often.
pub const MAX_WAIT: Duration = Duration::from_secs(2);

/// Watch `files` (each level and its file) and sync `local` on a change,
/// until `local` is gone. The watcher's own failure is logged and the
/// daemon goes on without following the files.
pub fn spawn(
    local: Weak<super::local::Local>,
    files: Vec<(Level, PathBuf)>,
) -> Option<tokio::task::JoinHandle<()>> {
    let (tx, rx) = mpsc::unbounded_channel();
    match Watch::new(files, tx) {
        Ok(watch) => Some(tokio::spawn(follow(local, rx, watch))),
        Err(err) => {
            tracing::warn!(%err, "the settings files are not followed while the daemon runs");
            None
        }
    }
}

/// The watcher and what it watches: each file's directory, or, while that
/// does not exist (a project with no `.rkat/` yet, a user with no
/// `~/.rkat/tui/`), its nearest existing ancestor, until the directory
/// appears ([`Self::arm`]).
struct Watch {
    watcher: notify::RecommendedWatcher,
    files: Vec<(Level, PathBuf)>,
    watched: HashSet<PathBuf>,
}

impl Watch {
    /// A watcher of `files` armed ([`Self::arm`]), sending the paths of
    /// every event but a mere access to `tx`.
    fn new(
        files: Vec<(Level, PathBuf)>,
        tx: mpsc::UnboundedSender<Vec<PathBuf>>,
    ) -> Result<Self, notify::Error> {
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event
                && !matches!(event.kind, notify::EventKind::Access(_))
            {
                let _ = tx.send(event.paths);
            }
        })?;
        let mut watch = Self {
            watcher,
            files,
            watched: HashSet::new(),
        };
        watch.arm()?;
        Ok(watch)
    }

    /// Watch, for each file, its directory or its nearest existing
    /// ancestor (non-recursive), where none is watched yet. The levels
    /// whose own directory is watched from now on.
    fn arm(&mut self) -> Result<Vec<Level>, notify::Error> {
        use notify::Watcher as _;
        let mut armed = Vec::new();
        for (level, file) in &self.files {
            let Some(dir) = file.parent() else {
                continue;
            };
            // Deeper while directories appear: one made just before its
            // parent's watch was placed sends that watch nothing.
            while let Some(target) = dir.ancestors().find(|at| at.is_dir())
                && !self.watched.contains(target)
            {
                self.watcher
                    .watch(target, notify::RecursiveMode::NonRecursive)?;
                self.watched.insert(target.to_path_buf());
                if target == dir {
                    armed.push(*level);
                }
            }
        }
        Ok(armed)
    }

    /// Whether every file's own directory is watched.
    fn armed(&self) -> bool {
        self.files
            .iter()
            .all(|(_, file)| file.parent().is_some_and(|dir| self.watched.contains(dir)))
    }

    /// The levels whose file one of `paths` names; a directory that
    /// appeared on the way to a file is watched now, and its level read
    /// (the file may have been written already).
    fn levels(&mut self, paths: &[PathBuf]) -> Vec<Level> {
        let mut levels: Vec<Level> = self
            .files
            .iter()
            .filter(|(_, file)| paths.iter().any(|path| same_file(path, file)))
            .map(|(level, _)| *level)
            .collect();
        if !self.armed() {
            match self.arm() {
                Ok(armed) => levels.extend(armed),
                Err(err) => tracing::warn!(%err, "a settings file's directory is not followed"),
            }
        }
        levels
    }
}

/// Whether `event` names `file` (by its name in the same directory: a
/// watched directory's events carry its path as the watch was given).
fn same_file(event: &Path, file: &Path) -> bool {
    event.file_name() == file.file_name()
        && match (event.parent(), file.parent()) {
            (Some(a), Some(b)) => a == b || a.canonicalize().ok() == b.canonicalize().ok(),
            _ => false,
        }
}

async fn follow(
    local: Weak<super::local::Local>,
    mut rx: mpsc::UnboundedReceiver<Vec<PathBuf>>,
    mut watch: Watch,
) {
    while let Some(first) = rx.recv().await {
        let mut paths = first;
        paths.extend(
            quiet(&mut rx, DEBOUNCE, MAX_WAIT)
                .await
                .into_iter()
                .flatten(),
        );
        let mut levels = watch.levels(&paths);
        levels.sort();
        levels.dedup();
        if levels.is_empty() {
            continue;
        }
        let Some(local) = local.upgrade() else {
            return;
        };
        for level in levels {
            local.sync_settings(level).await;
        }
    }
}

/// What arrives on `rx` until it is quiet for `debounce`, or `max` has
/// passed since the call.
async fn quiet<T>(
    rx: &mut mpsc::UnboundedReceiver<T>,
    debounce: Duration,
    max: Duration,
) -> Vec<T> {
    let deadline = tokio::time::Instant::now() + max;
    let mut got = Vec::new();
    loop {
        let wait = debounce.min(deadline.saturating_duration_since(tokio::time::Instant::now()));
        if wait.is_zero() {
            return got;
        }
        match tokio::time::timeout(wait, rx.recv()).await {
            Ok(Some(item)) => got.push(item),
            Ok(None) | Err(_) => return got,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A file whose directory did not exist when the watch started (a
    /// project with no `.rkat/` yet, a user with no `~/.rkat/tui/`) is
    /// followed once the directory appears: the nearest existing directory
    /// is watched until then. Before, such a file was never followed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_file_whose_directory_appears_later_is_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("project").join(".rkat");
        let file = dir.join("tui.toml");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut watch = Watch::new(vec![(Level::Project, file.clone())], tx).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "[ui]\nbell = false\n").unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let paths = rx.recv().await.unwrap();
                if watch.levels(&paths).contains(&Level::Project) {
                    return;
                }
            }
        })
        .await
        .expect("the file was never named");
    }

    /// A file touched more often than the quiet time is still read: the
    /// wait for quiet ends at [`MAX_WAIT`]; a quiet file ends it at once.
    #[tokio::test(start_paused = true)]
    async fn the_wait_for_quiet_is_capped() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let writer = tokio::spawn(async move {
            for n in 0..100 {
                if tx.send(n).is_err() {
                    return;
                }
                tokio::time::sleep(DEBOUNCE / 2).await;
            }
        });
        let start = tokio::time::Instant::now();
        let got = quiet(&mut rx, DEBOUNCE, MAX_WAIT).await;
        assert_eq!(start.elapsed(), MAX_WAIT);
        assert!(got.len() >= 10, "{got:?}");
        writer.abort();
        let (_tx, mut rx) = mpsc::unbounded_channel::<u8>();
        let start = tokio::time::Instant::now();
        assert!(quiet(&mut rx, DEBOUNCE, MAX_WAIT).await.is_empty());
        assert_eq!(start.elapsed(), DEBOUNCE);
    }
}
