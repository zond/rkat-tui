//! A realm directory that came with the repository is refused.
//!
//! The workspace realm (`<workspace>/.rkat/tui/`) holds the realm config,
//! the realm rules, the sessions and the approvals. rkat-tui writes a
//! `.gitignore` into it the first time it creates it, so a directory that
//! exists without one, or any file under it that git tracks, was not made
//! by rkat-tui on this host: it arrived with a clone. [`check`] refuses
//! the start then, naming what it found and how to proceed (`git rm -r
//! --cached`, or `--trust-project` to start anyway).
//!
//! "Tracked" is read from the git index itself (no `git` subprocess): the
//! index of the nearest repository at or above the workspace, through a
//! `.git` file's `gitdir:` pointer for a worktree or submodule. A missing
//! index tracks nothing; an index that cannot be read or parsed refuses the
//! start (it may track the realm, and that cannot be told).

use std::path::{Path, PathBuf};

/// What [`check`] found.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Committed {
    /// The realm directory holds files but not the `.gitignore` rkat-tui
    /// writes when it creates it.
    #[error(
        "{} holds files but no .gitignore, so rkat-tui did not create it here; it may have come with the repository and could set rules, endpoints or credentials. If it is yours, restore its .gitignore (printf '*\\n' > {}/.gitignore); if it came with the repository, remove it. --trust-project starts anyway",
        .realm.display(),
        .realm.display()
    )]
    NoGitignore {
        /// The realm directory.
        realm: PathBuf,
    },
    /// Git tracks files under the realm directory.
    #[error(
        "git tracks {} file(s) under {} ({}): a committed realm can set rules, endpoints or credentials for whoever clones it. Untrack it with `git rm -r --cached {}` (and commit), or start anyway with --trust-project",
        .files.len(),
        .realm.display(),
        .files.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
        .relative.display()
    )]
    Tracked {
        /// The realm directory.
        realm: PathBuf,
        /// It, relative to the repository's top level.
        relative: PathBuf,
        /// The tracked paths, as the index names them.
        files: Vec<String>,
    },
    /// The git index cannot be read, so whether it tracks the realm cannot
    /// be told.
    #[error(
        "cannot tell whether git tracks {}: the index {} cannot be read ({reason}); --trust-project starts anyway",
        .realm.display(),
        .index.display()
    )]
    Unreadable {
        /// The realm directory.
        realm: PathBuf,
        /// The index file.
        index: PathBuf,
        /// Why.
        reason: String,
    },
}

/// Refuse a realm directory `realm` (inside `workspace`) that came with
/// the repository (see the module docs). Run before rkat-tui creates the
/// directory or its `.gitignore`.
///
/// # Errors
///
/// [`Committed`] naming what was found.
pub fn check(workspace: &Path, realm: &Path) -> Result<(), Committed> {
    if realm.is_dir() && !realm.join(".gitignore").exists() {
        let holds_files = std::fs::read_dir(realm)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(true);
        if holds_files {
            return Err(Committed::NoGitignore {
                realm: realm.to_path_buf(),
            });
        }
    }
    let Some((top, git_dir)) = repository(workspace) else {
        return Ok(());
    };
    let Ok(relative) = realm.strip_prefix(&top) else {
        return Ok(());
    };
    let index = git_dir.join("index");
    let unreadable = |reason: String| Committed::Unreadable {
        realm: realm.to_path_buf(),
        index: index.clone(),
        reason,
    };
    let bytes = match std::fs::read(&index) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(unreadable(err.to_string())),
    };
    let hash_len = if sha256_repository(&git_dir) { 32 } else { 20 };
    let mut prefix = relative.to_string_lossy().replace('\\', "/");
    prefix.push('/');
    let files = tracked_under(&bytes, &prefix, hash_len).map_err(unreadable)?;
    if files.is_empty() {
        Ok(())
    } else {
        Err(Committed::Tracked {
            realm: realm.to_path_buf(),
            relative: relative.to_path_buf(),
            files,
        })
    }
}

/// The nearest repository at or above `start`: its top level and its git
/// directory (a `.git` directory, or the target of a `.git` file's
/// `gitdir:` line).
fn repository(start: &Path) -> Option<(PathBuf, PathBuf)> {
    for dir in start.ancestors() {
        let entry = dir.join(".git");
        let Ok(meta) = std::fs::metadata(&entry) else {
            continue;
        };
        if meta.is_dir() {
            return Some((dir.to_path_buf(), entry));
        }
        let text = std::fs::read_to_string(&entry).unwrap_or_default();
        let target = text
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("gitdir:"))
            .map(str::trim)
            .filter(|target| !target.is_empty());
        // An unreadable pointer leaves the git directory unknown: its
        // index then reads as unreadable, which refuses.
        let git_dir = target.map_or_else(|| entry.clone(), |target| dir.join(target));
        return Some((dir.to_path_buf(), git_dir));
    }
    None
}

/// Whether the repository whose git directory is `git_dir` uses SHA-256
/// object names (`extensions.objectformat = sha256`, in its own config or
/// the shared one of a worktree).
fn sha256_repository(git_dir: &Path) -> bool {
    let mut configs = vec![git_dir.join("config")];
    if let Ok(common) = std::fs::read_to_string(git_dir.join("commondir")) {
        configs.push(git_dir.join(common.trim()).join("config"));
    }
    configs.iter().any(|config| {
        std::fs::read_to_string(config).is_ok_and(|text| {
            text.lines().any(|line| {
                let line: String = line.chars().filter(|c| !c.is_whitespace()).collect();
                line.eq_ignore_ascii_case("objectformat=sha256")
            })
        })
    })
}

/// The paths of the git index `bytes` (versions 2 to 4) that lie under
/// `prefix` (a repository-relative directory ending in `/`), or a sparse
/// directory entry that holds it.
///
/// # Errors
///
/// A description of the first malformation.
pub fn tracked_under(bytes: &[u8], prefix: &str, hash_len: usize) -> Result<Vec<String>, String> {
    let u32_at = |pos: usize| -> Result<u32, String> {
        bytes
            .get(pos..pos + 4)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_be_bytes)
            .ok_or_else(|| "truncated".to_string())
    };
    if bytes.get(..4) != Some(b"DIRC") {
        return Err("not a git index".to_string());
    }
    let version = u32_at(4)?;
    if !(2..=4).contains(&version) {
        return Err(format!("unsupported index version {version}"));
    }
    let count = u32_at(8)?;
    let mut pos = 12_usize;
    let mut previous: Vec<u8> = Vec::new();
    let mut found = Vec::new();
    for _ in 0..count {
        let start = pos;
        let flags_at = start + 40 + hash_len;
        let flags = bytes
            .get(flags_at..flags_at + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .ok_or_else(|| "truncated entry".to_string())?;
        let mut name_at = flags_at + 2;
        if version >= 3 && flags & 0x4000 != 0 {
            name_at += 2;
        }
        let path = if version == 4 {
            let (strip, after) = varint(bytes, name_at)?;
            let end = nul(bytes, after)?;
            let keep = previous
                .len()
                .checked_sub(strip)
                .ok_or_else(|| "bad path compression".to_string())?;
            let mut path = previous[..keep].to_vec();
            path.extend_from_slice(&bytes[after..end]);
            pos = end + 1;
            path
        } else {
            let end = nul(bytes, name_at)?;
            let path = bytes[name_at..end].to_vec();
            // Entries are padded with 1 to 8 NULs to a multiple of 8.
            pos = start + ((end - start + 8) & !7);
            path
        };
        let name = String::from_utf8_lossy(&path).into_owned();
        if name.starts_with(prefix) || (name.ends_with('/') && prefix.starts_with(&name)) {
            found.push(name);
        }
        previous = path;
    }
    Ok(found)
}

/// The offset of the NUL that ends the name starting at `from`.
fn nul(bytes: &[u8], from: usize) -> Result<usize, String> {
    bytes
        .get(from..)
        .and_then(|rest| rest.iter().position(|b| *b == 0))
        .map(|offset| from + offset)
        .ok_or_else(|| "unterminated path".to_string())
}

/// Git's offset varint at `pos`: its value and the position after it.
fn varint(bytes: &[u8], mut pos: usize) -> Result<(usize, usize), String> {
    let mut byte = *bytes.get(pos).ok_or("truncated varint")?;
    pos += 1;
    let mut value = usize::from(byte & 0x7f);
    while byte & 0x80 != 0 {
        byte = *bytes.get(pos).ok_or("truncated varint")?;
        pos += 1;
        value = value
            .checked_add(1)
            .and_then(|v| v.checked_mul(128))
            .and_then(|v| v.checked_add(usize::from(byte & 0x7f)))
            .ok_or("varint overflow")?;
    }
    Ok((value, pos))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_realm_without_its_gitignore_is_refused_and_an_empty_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let realm = dir.path().join(".rkat/tui");
        std::fs::create_dir_all(&realm).unwrap();
        assert_eq!(check(dir.path(), &realm), Ok(()));
        std::fs::write(realm.join("config.toml"), "").unwrap();
        assert!(matches!(
            check(dir.path(), &realm),
            Err(Committed::NoGitignore { .. })
        ));
        std::fs::write(realm.join(".gitignore"), "*\n").unwrap();
        assert_eq!(check(dir.path(), &realm), Ok(()));
    }

    #[test]
    fn an_unreadable_index_refuses_rather_than_passing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/index"), b"DIRC\0\0\0\x09garbage").unwrap();
        let realm = root.join(".rkat/tui");
        assert!(matches!(
            check(root, &realm),
            Err(Committed::Unreadable { .. })
        ));
        // No index at all: nothing is tracked.
        std::fs::remove_file(root.join(".git/index")).unwrap();
        assert_eq!(check(root, &realm), Ok(()));
    }
}
