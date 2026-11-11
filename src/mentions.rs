//! `@path` mentions: the workspace paths the input's completion menu offers,
//! and the expansion that attaches the mentioned files to a prompt.
//!
//! A mention is a word that starts with `@` (so `a@b` is not one) and names
//! a file or directory inside the workspace. The transcript shows the prompt
//! as typed; the session receives [`expand_mentions`]'s text, the prompt
//! followed by one `<file>` block per mentioned file and one `<directory>`
//! listing per mentioned directory. Sealed paths ([`ResolvedPath::sealed`](crate::paths::ResolvedPath::sealed):
//! `.git`, `.rkat`, the realm and the user config directory) are never read.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::paths::ProjectRoot;

/// Bytes of one file attached at most (more is cut with `[truncated]`).
pub const MAX_FILE_BYTES: usize = 64 * 1024;
/// Bytes attached to one prompt at most, every file and listing together.
pub const MAX_TOTAL_BYTES: usize = 256 * 1024;
/// Entries of an attached directory listing at most.
pub const MAX_DIR_ENTRIES: usize = 200;
/// Paths the completion index holds at most.
pub const MAX_INDEX_PATHS: usize = 100_000;

/// A workspace path the completion menu offers: relative, `/`-separated, a
/// directory with a trailing `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEntry {
    /// The path as inserted after the `@`.
    pub path: String,
    /// The path lowercased once, at index time: matching a fragment
    /// allocates nothing per entry.
    lower: String,
}

impl PathEntry {
    /// The entry for `path` (relative, `/`-separated, a directory with a
    /// trailing `/`).
    #[must_use]
    pub fn new(path: String) -> Self {
        let lower = path.to_lowercase();
        Self { path, lower }
    }

    /// The last component of the lowercased path, a directory's without
    /// its `/` (lowercasing adds no `/`).
    fn lower_name(&self) -> &str {
        let path = self.lower.trim_end_matches('/');
        path.rsplit('/').next().unwrap_or(path)
    }

    /// Whether the entry is a directory.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.path.ends_with('/')
    }

    fn depth(&self) -> usize {
        self.path.trim_end_matches('/').matches('/').count()
    }
}

/// Every file and directory of the workspace the way `glob` sees it
/// (`.gitignore` honoured, hidden files included, `.git` skipped) minus the
/// sealed paths, shallow ones first, then by path; at most
/// [`MAX_INDEX_PATHS`].
#[must_use]
pub fn workspace_paths(root: &ProjectRoot) -> Vec<PathEntry> {
    let base = root.as_path();
    // A parallel walk: a 20 000-file tree takes a fraction of the serial
    // walk's time.
    let found = std::sync::Mutex::new(Vec::new());
    crate::tools::mention_walker(root, base)
        .build_parallel()
        .run(|| {
            Box::new(|entry| {
                let Some(entry) = entry.ok() else {
                    return ignore::WalkState::Continue;
                };
                let Some(path) = index_path(base, &entry) else {
                    return ignore::WalkState::Continue;
                };
                let Ok(mut found) = found.lock() else {
                    return ignore::WalkState::Quit;
                };
                if found.len() >= MAX_INDEX_PATHS {
                    return ignore::WalkState::Quit;
                }
                found.push(PathEntry::new(path));
                ignore::WalkState::Continue
            })
        });
    let mut out = found.into_inner().unwrap_or_default();
    out.sort_by(|a, b| a.depth().cmp(&b.depth()).then_with(|| a.path.cmp(&b.path)));
    out
}

/// `entry`'s path relative to `base`, `/`-separated, a directory with a
/// trailing `/`; `None` for the root itself and for a name that is not
/// UTF-8.
fn index_path(base: &Path, entry: &ignore::DirEntry) -> Option<String> {
    let file_type = entry.file_type()?;
    let relative = entry
        .path()
        .strip_prefix(base)
        .ok()
        .and_then(Path::to_str)
        .filter(|rel| !rel.is_empty())?;
    // A backslash is a filename byte on Unix, not a separator.
    let mut path = relative.to_string();
    if file_type.is_dir() {
        path.push('/');
    }
    Some(path)
}

/// The entries `fragment` (the text after `@`) completes to, at most
/// `limit`: first those whose path starts with it, then those whose name
/// contains it (both case-insensitive). The directory the fragment already
/// names is left out, so `src/` offers what is in `src`.
#[must_use]
pub fn match_paths<'a>(index: &'a [PathEntry], fragment: &str, limit: usize) -> Vec<&'a PathEntry> {
    let needle = fragment.to_lowercase();
    let mut out: Vec<&PathEntry> = index
        .iter()
        .filter(|entry| {
            !(entry.is_dir() && entry.path == fragment) && entry.lower.starts_with(&needle)
        })
        .take(limit)
        .collect();
    if !needle.is_empty() && out.len() < limit {
        let more: Vec<&PathEntry> = index
            .iter()
            .filter(|entry| {
                entry.lower_name().contains(&needle)
                    && !out.iter().any(|seen| std::ptr::eq(*seen, *entry))
            })
            .take(limit - out.len())
            .collect();
        out.extend(more);
    }
    out
}

#[cfg(test)]
mod match_tests {
    use super::*;

    /// Matching folds case through the stored lowercase path: prefix
    /// first, then name, a directory the fragment names left out.
    #[test]
    fn matching_folds_case_by_prefix_then_name() {
        let index: Vec<PathEntry> = ["Src/", "Src/Main.rs", "docs/MAIN.md", "README.md"]
            .into_iter()
            .map(|p| PathEntry::new(p.to_string()))
            .collect();
        let hits = |fragment: &str| -> Vec<&str> {
            match_paths(&index, fragment, 10)
                .into_iter()
                .map(|e| e.path.as_str())
                .collect()
        };
        assert_eq!(hits("src/"), ["Src/", "Src/Main.rs"]);
        assert_eq!(hits("Src/"), ["Src/Main.rs"]);
        assert_eq!(hits("main"), ["Src/Main.rs", "docs/MAIN.md"]);
        assert_eq!(hits("READ"), ["README.md"]);
    }
}

/// What a mention names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionKind {
    /// A regular file inside the workspace.
    File,
    /// A directory inside the workspace.
    Dir,
    /// A sealed path: never read.
    Sealed,
}

/// One `@path` of a prompt that names something in the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// The path relative to the workspace (a directory with a trailing `/`).
    pub path: String,
    /// The resolved absolute path.
    pub abs: PathBuf,
    /// What it names.
    pub kind: MentionKind,
}

/// Punctuation a mention may be followed by in prose (`see @a.rs.`).
const TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'', '`'];

/// The mentions of `text`, each path once, in order: words starting with
/// `@` that resolve inside the workspace to a file, a directory or a sealed
/// path that exists. A word that names nothing (an `@user`) is no mention;
/// trailing punctuation is dropped when the word with it names nothing.
#[must_use]
pub fn scan_mentions(text: &str, root: &ProjectRoot) -> Vec<Mention> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for word in text.split_whitespace() {
        let Some(raw) = word.strip_prefix('@') else {
            continue;
        };
        let trimmed = raw.trim_end_matches(TRAILING);
        let Some(mention) = [raw, trimmed]
            .into_iter()
            .filter(|candidate| !candidate.is_empty())
            .find_map(|candidate| resolve(candidate, root))
        else {
            continue;
        };
        if seen.insert(mention.abs.clone()) {
            out.push(mention);
        }
    }
    out
}

fn resolve(raw: &str, root: &ProjectRoot) -> Option<Mention> {
    let resolved = root.resolve(raw).ok()?;
    let relative = resolved.relative.as_deref()?.to_str()?.to_string();
    if relative.is_empty() {
        return None;
    }
    let meta = std::fs::metadata(&resolved.abs).ok()?;
    let kind = if resolved.sealed {
        MentionKind::Sealed
    } else if meta.is_file() {
        MentionKind::File
    } else if meta.is_dir() {
        MentionKind::Dir
    } else {
        return None;
    };
    let path = if meta.is_dir() {
        format!("{relative}/")
    } else {
        relative
    };
    Some(Mention {
        path,
        abs: resolved.abs,
        kind,
    })
}

/// A prompt with its mentions attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    /// What the session receives: the prompt, then one block per attached
    /// file or directory (the prompt unchanged when there is none).
    pub text: String,
    /// The paths attached (a block each, a note included).
    pub attached: Vec<String>,
    /// The sealed paths mentioned, not read.
    pub sealed: Vec<String>,
}

/// Attach the files and directories `text` mentions ([`scan_mentions`]):
/// after the prompt, a `<file path="…">` block with each file's content
/// (UTF-8 only, at most [`MAX_FILE_BYTES`] with a `[truncated]` line) or a
/// one-line `<file path="…" note="…" />` for a binary file or one past the
/// [`MAX_TOTAL_BYTES`] budget, and a `<directory path="…">` listing (one
/// entry per line, directories with `/`, at most [`MAX_DIR_ENTRIES`]) per
/// directory. Sealed paths are listed in [`Expanded::sealed`] and never
/// read.
#[must_use]
pub fn expand_mentions(text: &str, root: &ProjectRoot) -> Expanded {
    let mut blocks = String::new();
    let mut attached = Vec::new();
    let mut sealed = Vec::new();
    let mut budget = MAX_TOTAL_BYTES;
    for mention in scan_mentions(text, root) {
        let block = match mention.kind {
            MentionKind::Sealed => {
                sealed.push(mention.path);
                continue;
            }
            MentionKind::File => file_block(&mention, &mut budget),
            MentionKind::Dir => dir_block(&mention, root, &mut budget),
        };
        blocks.push_str("\n\n");
        blocks.push_str(&block);
        attached.push(mention.path);
    }
    Expanded {
        text: format!("{text}{blocks}"),
        attached,
        sealed,
    }
}

/// The separators [`expand_mentions`] puts before each attached block.
const BLOCK_OPENERS: [&str; 2] = ["\n\n<file path=\"", "\n\n<directory path=\""];

/// A prompt split into what the user typed and the blocks
/// [`expand_mentions`] attached after it (`None`: nothing attached). The
/// split is at the first block opener, so everything of an attached file
/// is in the second part; a prompt that types an opener itself only gives
/// less to the first part.
#[must_use]
pub fn split_attachments(text: &str) -> (&str, Option<&str>) {
    match BLOCK_OPENERS
        .iter()
        .filter_map(|opener| text.find(opener))
        .min()
    {
        Some(at) => (&text[..at], Some(&text[at + 2..])),
        None => (text, None),
    }
}

/// Longest path label [`attached_paths`] returns, in characters.
const MAX_LABEL_CHARS: usize = 120;

/// The paths the attached `blocks` ([`split_attachments`]) name at the
/// start of a block, at most `max`, each on one line (control characters
/// are `?`) and at most [`MAX_LABEL_CHARS`] long (a label for the
/// classifier: file content that imitates an opener can only add a short
/// one-line name).
#[must_use]
pub fn attached_paths(blocks: &str, max: usize) -> Vec<String> {
    let mut paths = Vec::new();
    for block in std::iter::once(blocks).chain(blocks.split("\n\n").skip(1)) {
        let rest = block
            .strip_prefix("<file path=\"")
            .or_else(|| block.strip_prefix("<directory path=\""));
        if let Some(path) = rest.and_then(|rest| rest.split('"').next()) {
            // A label on one line: an opener planted in file content cannot
            // start a transcript line of its own (`\n[user] …`).
            let path: String = path
                .replace("&quot;", "\"")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&")
                .chars()
                .map(|c| if c.is_control() { '?' } else { c })
                .take(MAX_LABEL_CHARS)
                .collect();
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        if paths.len() == max {
            break;
        }
    }
    paths
}

/// `s` with `&`, `"`, `<` and `>` escaped, for an attribute value.
fn attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn note(tag: &str, path: &str, note: &str) -> String {
    format!("<{tag} path=\"{}\" note=\"{}\" />", attr(path), attr(note))
}

fn file_block(mention: &Mention, budget: &mut usize) -> String {
    let path = &mention.path;
    let cap = MAX_FILE_BYTES.min(*budget);
    if cap == 0 {
        return note(
            "file",
            path,
            "not attached: the 256 KiB attachment budget is used up",
        );
    }
    let mut bytes = Vec::new();
    let read = std::fs::File::open(&mention.abs)
        .and_then(|file| file.take(cap as u64 + 1).read_to_end(&mut bytes));
    if let Err(err) = read {
        return note("file", path, &format!("not attached: {err}"));
    }
    let truncated = bytes.len() > cap;
    bytes.truncate(cap);
    if bytes.contains(&0) {
        return note("file", path, "not attached: binary file");
    }
    let content = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        // A cut through the last character is not binary.
        Err(err) if truncated && err.error_len().is_none() => {
            std::str::from_utf8(bytes.get(..err.valid_up_to()).unwrap_or_default())
                .unwrap_or_default()
        }
        Err(_) => return note("file", path, "not attached: not UTF-8 text"),
    };
    *budget -= content.len();
    let mut out = format!("<file path=\"{}\">\n{content}", attr(path));
    if !content.is_empty() && !content.ends_with('\n') {
        out.push('\n');
    }
    if truncated {
        out.push_str("[truncated]\n");
    }
    out.push_str("</file>");
    out
}

fn dir_block(mention: &Mention, root: &ProjectRoot, budget: &mut usize) -> String {
    let path = &mention.path;
    let mut entries: Vec<String> = crate::tools::mention_walker(root, &mention.abs)
        .max_depth(Some(1))
        .build()
        .flatten()
        .filter(|entry| entry.depth() == 1)
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            Some(if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                format!("{name}/")
            } else {
                name
            })
        })
        .collect();
    entries.sort();
    let total = entries.len();
    let mut listing = String::new();
    for entry in entries.iter().take(MAX_DIR_ENTRIES) {
        let _ = writeln!(listing, "{entry}");
    }
    if total > MAX_DIR_ENTRIES {
        let _ = writeln!(listing, "[… and {} more]", total - MAX_DIR_ENTRIES);
    }
    if listing.len() > *budget {
        return note(
            "directory",
            path,
            "not attached: the 256 KiB attachment budget is used up",
        );
    }
    *budget -= listing.len();
    format!("<directory path=\"{}\">\n{listing}</directory>", attr(path))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, ProjectRoot) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("src/ui")).unwrap();
        std::fs::create_dir_all(p.join(".rkat")).unwrap();
        std::fs::create_dir_all(p.join(".git")).unwrap();
        std::fs::create_dir_all(p.join("target/debug")).unwrap();
        std::fs::write(p.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(p.join("README.md"), "# readme\n").unwrap();
        std::fs::write(p.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(p.join("src/ui/render.rs"), "// render\n").unwrap();
        std::fs::write(p.join(".rkat/config.toml"), "secret = 1\n").unwrap();
        std::fs::write(p.join(".git/HEAD"), "ref\n").unwrap();
        std::fs::write(p.join("target/debug/app"), "bin").unwrap();
        std::fs::write(p.join("logo.png"), [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
        std::fs::write(p.join("big.txt"), "é".repeat(MAX_FILE_BYTES)).unwrap();
        let root = ProjectRoot::new(p).unwrap();
        (dir, root)
    }

    fn paths(entries: &[&PathEntry]) -> Vec<String> {
        entries.iter().map(|e| e.path.clone()).collect()
    }

    #[test]
    fn the_index_honours_gitignore_and_skips_sealed_paths() {
        let (_dir, root) = workspace();
        let index = workspace_paths(&root);
        let all: Vec<&str> = index.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            all,
            [
                ".gitignore",
                "README.md",
                "big.txt",
                "logo.png",
                "src/",
                "src/main.rs",
                "src/ui/",
                "src/ui/render.rs",
            ]
        );
    }

    #[test]
    fn matching_is_path_prefix_then_name_substring() {
        let (_dir, root) = workspace();
        let index = workspace_paths(&root);
        assert_eq!(
            paths(&match_paths(&index, "src/", 8)),
            ["src/main.rs", "src/ui/", "src/ui/render.rs"]
        );
        assert_eq!(
            paths(&match_paths(&index, "SRC/U", 8)),
            ["src/ui/", "src/ui/render.rs"]
        );
        // Prefix first, then names containing it.
        assert_eq!(
            paths(&match_paths(&index, "re", 8)),
            ["README.md", ".gitignore", "src/ui/render.rs"]
        );
        assert_eq!(paths(&match_paths(&index, "main", 8)), ["src/main.rs"]);
        assert_eq!(match_paths(&index, "", 3).len(), 3);
        assert!(match_paths(&index, "zz", 8).is_empty());
    }

    /// A backslash is a filename byte on Unix: a file named `a\b` is
    /// indexed and attached under its own name, not as `a/b`.
    #[test]
    fn a_backslash_in_a_file_name_stays_a_backslash() {
        let (dir, root) = workspace();
        std::fs::write(dir.path().join("a\\b.txt"), "slashed\n").unwrap();
        let index = workspace_paths(&root);
        assert!(
            index.iter().any(|e| e.path == "a\\b.txt"),
            "{:?}",
            paths(&index.iter().collect::<Vec<_>>())
        );
        assert!(!index.iter().any(|e| e.path == "a/b.txt"));
        let found = scan_mentions("see @a\\b.txt", &root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "a\\b.txt");
        assert_eq!(found[0].kind, MentionKind::File);
    }

    #[test]
    fn scanning_finds_words_that_start_with_at_and_name_something() {
        let (_dir, root) = workspace();
        let found = scan_mentions(
            "see @src/main.rs, @src/ and @README.md. not a@README.md, @nobody, @.rkat/config.toml @src/main.rs",
            &root,
        );
        let kinds: Vec<(&str, MentionKind)> =
            found.iter().map(|m| (m.path.as_str(), m.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("src/main.rs", MentionKind::File),
                ("src/", MentionKind::Dir),
                ("README.md", MentionKind::File),
                (".rkat/config.toml", MentionKind::Sealed),
            ]
        );
        assert!(scan_mentions("mail me at a@b.c", &root).is_empty());
        assert!(scan_mentions("@../outside", &root).is_empty());
    }

    #[test]
    fn expansion_attaches_files_listings_and_notes() {
        let (_dir, root) = workspace();
        let prompt = "fix @src/main.rs using @src/ui, not @logo.png @.rkat/config.toml @big.txt";
        let expanded = expand_mentions(prompt, &root);
        assert_eq!(
            expanded.attached,
            ["src/main.rs", "src/ui/", "logo.png", "big.txt"]
        );
        assert_eq!(expanded.sealed, [".rkat/config.toml"]);
        let (head, blocks) = expanded.text.split_at(prompt.len());
        assert_eq!(head, prompt);
        assert!(
            blocks.starts_with(
                "\n\n<file path=\"src/main.rs\">\nfn main() {}\n</file>\
                 \n\n<directory path=\"src/ui/\">\nrender.rs\n</directory>\
                 \n\n<file path=\"logo.png\" note=\"not attached: binary file\" />\
                 \n\n<file path=\"big.txt\">\n"
            ),
            "{blocks}"
        );
        // 64 KiB of two-byte chars: cut on a char boundary, marked.
        let big = blocks.split("<file path=\"big.txt\">\n").nth(1).unwrap();
        let content = big.strip_suffix("\n[truncated]\n</file>").unwrap();
        assert_eq!(content.len(), MAX_FILE_BYTES);
        assert!(content.chars().all(|c| c == 'é'));
        assert!(!expanded.text.contains("secret"));
        // No mention: the prompt unchanged.
        let plain = expand_mentions("no files here, a@b", &root);
        assert_eq!(plain.text, "no files here, a@b");
        assert!(plain.attached.is_empty());
    }

    #[test]
    fn the_total_budget_turns_later_files_into_notes() {
        let dir = tempfile::tempdir().unwrap();
        let mut prompt = String::from("read");
        for i in 0..5 {
            std::fs::write(
                dir.path().join(format!("f{i}.txt")),
                "x".repeat(MAX_FILE_BYTES + 10),
            )
            .unwrap();
            prompt.push_str(&format!(" @f{i}.txt"));
        }
        let root = ProjectRoot::new(dir.path()).unwrap();
        let expanded = expand_mentions(&prompt, &root);
        assert_eq!(expanded.attached.len(), 5);
        assert_eq!(expanded.text.matches("[truncated]").count(), 4);
        assert!(expanded.text.ends_with(
            "<file path=\"f4.txt\" note=\"not attached: the 256 KiB attachment budget is used up\" />"
        ));
        assert!(expanded.text.len() < prompt.len() + MAX_TOTAL_BYTES + 1024);
    }

    #[test]
    fn a_long_directory_listing_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("many")).unwrap();
        for i in 0..(MAX_DIR_ENTRIES + 5) {
            std::fs::write(dir.path().join(format!("many/{i:04}")), "").unwrap();
        }
        let root = ProjectRoot::new(dir.path()).unwrap();
        let expanded = expand_mentions("@many", &root);
        assert_eq!(
            expanded.text.matches('\n').count(),
            2 + 1 + MAX_DIR_ENTRIES + 1
        );
        assert!(expanded.text.contains("0199\n[… and 5 more]\n</directory>"));
    }

    /// Index build time on this repository's own tree and on a synthetic
    /// 20 000-file tree (`cargo nextest run --run-ignored only -E
    /// 'test(index_build_time)'`; the numbers go to docs/evidence.md).
    #[test]
    #[ignore = "timing measurement, run by hand"]
    fn index_build_time() {
        let repo = ProjectRoot::new(env!("CARGO_MANIFEST_DIR")).unwrap();
        let started = std::time::Instant::now();
        let n = workspace_paths(&repo).len();
        println!("this repo: {n} paths in {:?}", started.elapsed());
        let dir = tempfile::tempdir().unwrap();
        for d in 0..200 {
            let sub = dir.path().join(format!("d{d:03}"));
            std::fs::create_dir(&sub).unwrap();
            for f in 0..100 {
                std::fs::write(sub.join(format!("f{f:03}.rs")), "").unwrap();
            }
        }
        let synthetic = ProjectRoot::new(dir.path()).unwrap();
        let started = std::time::Instant::now();
        let n = workspace_paths(&synthetic).len();
        let built = started.elapsed();
        let index = workspace_paths(&synthetic);
        let started = std::time::Instant::now();
        let hits = match_paths(&index, "f05", 8).len();
        println!(
            "synthetic: {n} paths in {built:?}; a match ({hits} hits) in {:?}",
            started.elapsed()
        );
    }
}
