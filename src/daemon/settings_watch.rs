//! Following the settings files while the daemon runs: the user's own
//! `tui.toml`, the workspace's realm `tui.toml` and the project's
//! `.rkat/tui.toml` are watched (the `notify` crate, each file's directory,
//! without recursion; only events naming the file count), and a change is
//! read again after a short quiet time ([`DEBOUNCE`]) and synced
//! ([`super::local::Local::sync_settings`]): each setting whose value in
//! force changed runs its hook, the clients get one notice per setting,
//! and an audit line records it. The daemon's own writes find the file as
//! it holds it already and change nothing (compared by content, not by
//! timing); a file that does not parse is reported once and its last good
//! values stay in force.

use std::path::{Path, PathBuf};
use std::sync::Weak;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::config::registry::Level;

/// How long the files must be quiet before they are read again (an editor
/// writes in steps; two daemons' writes come close together).
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// Watch `files` (each level and its file) and sync `local` on a change,
/// until `local` is gone. The watcher's own failure is logged and the
/// daemon goes on without following the files.
pub fn spawn(
    local: Weak<super::local::Local>,
    files: Vec<(Level, PathBuf)>,
) -> Option<tokio::task::JoinHandle<()>> {
    let (tx, rx) = mpsc::unbounded_channel();
    let watcher = match watcher(&files, tx) {
        Ok(watcher) => watcher,
        Err(err) => {
            tracing::warn!(%err, "the settings files are not followed while the daemon runs");
            return None;
        }
    };
    Some(tokio::spawn(follow(local, rx, watcher)))
}

/// A watcher on each file's directory that sends the level of every event
/// naming the file.
fn watcher(
    files: &[(Level, PathBuf)],
    tx: mpsc::UnboundedSender<Level>,
) -> Result<notify::RecommendedWatcher, notify::Error> {
    use notify::Watcher as _;
    let watched: Vec<(Level, PathBuf)> = files.to_vec();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        for (level, file) in &watched {
            if event.paths.iter().any(|path| same_file(path, file)) {
                let _ = tx.send(*level);
            }
        }
    })?;
    for (_, file) in files {
        if let Some(dir) = file.parent().filter(|dir| dir.is_dir()) {
            watcher.watch(dir, notify::RecursiveMode::NonRecursive)?;
        }
    }
    Ok(watcher)
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
    mut rx: mpsc::UnboundedReceiver<Level>,
    _watcher: notify::RecommendedWatcher,
) {
    while let Some(first) = rx.recv().await {
        let mut levels = vec![first];
        while let Ok(Some(level)) = tokio::time::timeout(DEBOUNCE, rx.recv()).await {
            if !levels.contains(&level) {
                levels.push(level);
            }
        }
        let Some(local) = local.upgrade() else {
            return;
        };
        for level in levels {
            local.sync_settings(level).await;
        }
    }
}
