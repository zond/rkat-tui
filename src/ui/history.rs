//! Persistent prompt history: one entry per line, newest last.
//!
//! Entries are escaped so multi-line prompts fit on one line (`\n`, `\r` and `\\`
//! become two-character escapes). Consecutive duplicates are dropped and the file
//! is capped at [`MAX_ENTRIES`], rewritten when it grows past the cap.
//!
//! Several clients share a file (two in one workspace; the global one with
//! every rkat-tui of the user). Each appends its own lines, and a rewrite
//! reads the file again first, so it keeps what the others appended since
//! this one loaded (a line appended between that read and the rename is
//! the window left). The files hold every prompt typed: owner-only.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// Maximum number of entries kept on disk and in memory.
pub const MAX_ENTRIES: usize = 1000;

/// Prompt history backed by a line-oriented file.
#[derive(Debug, Clone)]
pub struct History {
    path: PathBuf,
    entries: Vec<String>,
    /// Number of lines currently in the file (may exceed `entries.len()` before a rewrite).
    file_lines: usize,
    /// Whether the file's last line lacks a terminating newline.
    missing_newline: bool,
}

impl History {
    /// Load history from `path`. A missing or unreadable file yields an empty
    /// history (unreadable files are logged, never surfaced as errors).
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut missing_newline = false;
        let (entries, file_lines) = match fs::read_to_string(&path) {
            Ok(content) => {
                missing_newline = !content.is_empty() && !content.ends_with('\n');
                let mut entries: Vec<String> = Vec::new();
                let mut lines = 0usize;
                for line in content.lines() {
                    lines += 1;
                    if line.is_empty() {
                        continue;
                    }
                    let entry = unescape(line);
                    if entries.last() != Some(&entry) {
                        entries.push(entry);
                    }
                }
                let excess = entries.len().saturating_sub(MAX_ENTRIES);
                entries.drain(..excess);
                (entries, lines)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (Vec::new(), 0),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read prompt history");
                (Vec::new(), 0)
            }
        };
        Self {
            path,
            entries,
            file_lines,
            missing_newline,
        }
    }

    /// File backing this history.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Entries, oldest first.
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Consume the history, returning its entries (oldest first), e.g. for
    /// [`super::App::with_history`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn into_entries(self) -> Vec<String> {
        self.entries
    }

    /// Append `entry` and persist it. Blank entries and repeats of the newest
    /// entry are ignored. When the file would exceed [`MAX_ENTRIES`] lines it is
    /// rewritten with only the newest entries.
    pub fn append(&mut self, entry: &str) -> io::Result<()> {
        if entry.trim().is_empty() || self.entries.last().is_some_and(|last| last == entry) {
            return Ok(());
        }
        self.entries.push(entry.to_string());
        let excess = self.entries.len().saturating_sub(MAX_ENTRIES);
        self.entries.drain(..excess);
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        // A file an earlier build created with the default mode.
        if file.metadata()?.permissions().mode() & 0o077 != 0 {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let sep = if self.missing_newline { "\n" } else { "" };
        writeln!(file, "{sep}{}", escape(entry))?;
        self.missing_newline = false;
        self.file_lines += 1;
        if self.file_lines > MAX_ENTRIES {
            self.rewrite()?;
        }
        Ok(())
    }

    /// Cut the file to its newest [`MAX_ENTRIES`] lines, as the file holds
    /// them now (another client's lines included), by an owner-only
    /// atomic replace.
    fn rewrite(&mut self) -> io::Result<()> {
        let content = fs::read_to_string(&self.path)?;
        let lines: Vec<&str> = content.lines().filter(|line| !line.is_empty()).collect();
        let kept = &lines[lines.len().saturating_sub(MAX_ENTRIES)..];
        let mut body = kept.join("\n");
        body.push('\n');
        crate::paths::write_atomic(
            &self.path,
            body.as_bytes(),
            crate::paths::FileMode::Private,
            None,
        )
        .map_err(io::Error::other)?;
        self.file_lines = kept.len();
        self.missing_newline = false;
        Ok(())
    }
}

fn escape(entry: &str) -> String {
    let mut out = String::with_capacity(entry.len());
    for ch in entry.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("realm").join("tui_history")
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::load(path(&dir));
        assert!(h.entries().is_empty());
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::load(path(&dir));
        h.append("first").unwrap();
        h.append("second").unwrap();
        let loaded = History::load(path(&dir));
        assert_eq!(
            loaded.entries(),
            &["first".to_string(), "second".to_string()]
        );
        assert_eq!(loaded.into_entries().len(), 2);
    }

    #[test]
    fn dedupes_consecutive_repeats_and_skips_blank() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::load(path(&dir));
        for e in ["a", "a", "b", "  ", "", "a"] {
            h.append(e).unwrap();
        }
        assert_eq!(h.entries(), &["a", "b", "a"]);
        let content = fs::read_to_string(path(&dir)).unwrap();
        assert_eq!(content, "a\nb\na\n");
        assert_eq!(History::load(path(&dir)).entries(), &["a", "b", "a"]);
    }

    #[test]
    fn escapes_newlines_and_backslashes() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::load(path(&dir));
        let entry = "line one\nline two with \\n literal\r\nend\\";
        h.append(entry).unwrap();
        let content = fs::read_to_string(path(&dir)).unwrap();
        assert_eq!(content.lines().count(), 1);
        assert_eq!(History::load(path(&dir)).entries(), &[entry.to_string()]);
    }

    #[test]
    fn append_after_unterminated_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "old").unwrap();
        let mut h = History::load(&p);
        h.append("new").unwrap();
        assert_eq!(History::load(&p).entries(), &["old", "new"]);
    }

    #[test]
    fn caps_file_at_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        let mut h = History::load(&p);
        for i in 0..MAX_ENTRIES + 5 {
            h.append(&format!("p{i}")).unwrap();
        }
        assert_eq!(h.entries().len(), MAX_ENTRIES);
        let content = fs::read_to_string(&p).unwrap();
        assert!(content.lines().count() <= MAX_ENTRIES);
        let loaded = History::load(&p);
        assert_eq!(loaded.entries().len(), MAX_ENTRIES);
        assert_eq!(loaded.entries().first().map(String::as_str), Some("p5"));
        assert_eq!(
            loaded.entries().last().map(String::as_str),
            Some(format!("p{}", MAX_ENTRIES + 4).as_str())
        );
    }

    #[test]
    fn oversized_file_is_trimmed_on_load_and_rewritten_on_append() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let body = (0..MAX_ENTRIES + 50)
            .map(|i| format!("e{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&p, body).unwrap();
        let mut h = History::load(&p);
        assert_eq!(h.entries().len(), MAX_ENTRIES);
        h.append("new").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap().lines().count(), MAX_ENTRIES);
    }

    /// Two clients on one file: a rewrite keeps the lines the other
    /// appended since this one loaded, and the file is owner-only.
    #[test]
    fn a_rewrite_keeps_what_another_client_appended() {
        let dir = tempfile::tempdir().unwrap();
        let file = path(&dir);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut full = String::new();
        for n in 0..MAX_ENTRIES {
            full.push_str(&format!("old {n}\n"));
        }
        std::fs::write(&file, full).unwrap();
        let mut a = History::load(&file);
        let mut b = History::load(&file);
        b.append("from b").unwrap();
        a.append("from a").unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), MAX_ENTRIES);
        assert_eq!(lines[MAX_ENTRIES - 2..], ["from b", "from a"]);
        let loaded = History::load(&file);
        assert!(loaded.entries().iter().any(|e| e == "from b"));
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    /// A new file is created owner-only; an existing world-readable one
    /// becomes owner-only at the next append.
    #[test]
    fn history_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let file = path(&dir);
        let mut h = History::load(&file);
        h.append("first").unwrap();
        let mode = |f: &Path| std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        h.append("second").unwrap();
        assert_eq!(mode(&file), 0o600);
    }
}
