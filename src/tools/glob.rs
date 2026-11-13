//! `glob`: gitignore-aware file search by glob pattern, newest first.

use super::prune::Prune;
use super::{CallMeta, GLOB, Shared, ToolFailure, ToolOutcome};
use globset::GlobBuilder;
use ignore::WalkBuilder;
use meerkat_core::ToolDef;
use serde::Deserialize;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Maximum number of paths returned.
pub(crate) const MAX_RESULTS: usize = 500;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GlobArgs {
    #[schemars(
        description = "Glob pattern matched against paths relative to the search directory, e.g. `**/*.rs` or `src/*.toml`. `*` does not cross `/`; use `**` for any depth."
    )]
    pub pattern: String,
    #[schemars(description = "Directory to search (default: the project root).")]
    #[serde(default)]
    pub path: Option<String>,
}

pub(crate) fn definition() -> ToolDef {
    ToolDef::new(
        GLOB,
        format!(
            "Find files by glob pattern. Respects .gitignore; hidden files are included unless \
             ignored. Returns one path per line, most recently modified first, at most \
             {MAX_RESULTS} results."
        ),
        meerkat_tools::schema_for::<GlobArgs>(),
    )
}

/// Resolve the directory (or file) a search starts from.
pub(crate) fn search_root(
    shared: &Shared,
    tool: &str,
    path: Option<&str>,
) -> Result<PathBuf, ToolFailure> {
    let Some(raw) = path else {
        return Ok(shared.root.as_path().to_path_buf());
    };
    let resolved = shared.resolve(tool, raw)?;
    if !resolved.abs.exists() {
        return Err(ToolFailure::recoverable(format!(
            "path not found: {}",
            shared.display(&resolved)
        )));
    }
    Ok(resolved.abs)
}

/// A gitignore-aware walker that includes hidden files but never descends
/// into `.git` or a directory of `secret` (the project root's secret roots,
/// [`crate::paths::ProjectRoot::secret_roots`]); with `hide_secrets` it also
/// skips environment files.
pub(crate) fn walker(root: &Path, hide_secrets: bool, secret: &[PathBuf]) -> WalkBuilder {
    pruned_walker(root, hide_secrets, secret, |_| true)
}

/// [`walker`] that does not descend into a directory below `root` whose
/// path relative to `root` `enter` refuses (a search's pruning,
/// [`super::prune`]).
pub(crate) fn pruned_walker(
    root: &Path,
    hide_secrets: bool,
    secret: &[PathBuf],
    enter: impl Fn(&Path) -> bool + Send + Sync + 'static,
) -> WalkBuilder {
    let secret = secret.to_vec();
    let base = root.to_path_buf();
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .filter_entry(move |entry| {
            entry.file_name() != ".git"
                && !(hide_secrets && crate::paths::is_secret_file_name(entry.file_name()))
                && !secret.iter().any(|dir| entry.path().starts_with(dir))
                && (entry.depth() == 0
                    || !entry.file_type().is_some_and(|kind| kind.is_dir())
                    || entry.path().strip_prefix(&base).map_or(true, &enter))
        });
    builder
}

/// The walker `@` mentions use (the input's path menu and the attached
/// directory listings): as [`walker`] from `start`, without hiding secrets
/// (the user names the file), and it never enters a sealed path of `root`
/// ([`crate::paths::ProjectRoot::is_sealed_relative`] or one of its sealed
/// roots).
pub(crate) fn mention_walker(root: &crate::paths::ProjectRoot, start: &Path) -> WalkBuilder {
    let base = root.as_path().to_path_buf();
    let sealed = root.sealed_roots().to_vec();
    let mut builder = walker(start, false, root.secret_roots());
    let root = root.clone();
    builder.filter_entry(move |entry| {
        let path = entry.path();
        entry.file_name() != ".git"
            && !path
                .strip_prefix(&base)
                .is_ok_and(|rel| root.is_sealed_relative(rel))
            && !root.below_any(path, &sealed)
    });
    builder
}

/// One walk thread's matches: the newest [`MAX_RESULTS`] of them (the
/// worst on top) and how many it saw.
#[derive(Default)]
struct Newest {
    kept: BinaryHeap<Reverse<Hit>>,
    total: usize,
}

/// A match ordered by how early it is listed: newer first, then by path.
#[derive(PartialEq, Eq)]
struct Hit {
    mtime: SystemTime,
    path: PathBuf,
}

impl Ord for Hit {
    /// Greater is listed earlier: the newer, then the smaller path.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.mtime
            .cmp(&other.mtime)
            .then_with(|| other.path.cmp(&self.path))
    }
}

impl PartialOrd for Hit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Newest {
    fn keep(&mut self, hit: Hit) {
        self.total += 1;
        self.kept.push(Reverse(hit));
        if self.kept.len() > MAX_RESULTS {
            self.kept.pop();
        }
    }
}

pub(crate) fn run(shared: &Shared, call: CallMeta, args: GlobArgs) -> ToolOutcome {
    run_with(shared, call, args, true)
}

/// [`run`], its walk pruned by the pattern ([`Prune`]) when `prune`; an
/// unpruned walk is the tests' reference.
pub(crate) fn run_with(
    shared: &Shared,
    call: CallMeta,
    args: GlobArgs,
    prune: bool,
) -> ToolOutcome {
    let matcher = GlobBuilder::new(&args.pattern)
        .literal_separator(true)
        .build()
        .map_err(|err| ToolFailure::recoverable(format!("invalid glob pattern: {err}")))?
        .compile_matcher();
    let root = search_root(shared, GLOB, args.path.as_deref())?;
    if !root.is_dir() {
        return Err(ToolFailure::recoverable(format!(
            "{} is not a directory",
            shared.display_abs(&root)
        )));
    }

    let scan = call.scan(&root);
    let pruning = prune.then(|| Prune::compile(&args.pattern)).flatten();
    let walker = match pruning {
        Some(pruning) => pruned_walker(
            &root,
            shared.hide_secrets,
            shared.root.secret_roots(),
            move |relative| pruning.may_match_below(relative),
        ),
        None => walker(&root, shared.hide_secrets, shared.root.secret_roots()),
    };
    let found = scan.walk(&walker, |entry, newest: &mut Newest| {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            return true;
        }
        let Ok(relative) = entry.path().strip_prefix(&root) else {
            return true;
        };
        if matcher.is_match(relative) {
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            newest.keep(Hit {
                mtime,
                path: entry.path().to_path_buf(),
            });
        }
        true
    });
    let total: usize = found.iter().map(|newest| newest.total).sum();
    let mut hits: Vec<Hit> = found
        .into_iter()
        .flat_map(|newest| newest.kept.into_iter().map(|Reverse(hit)| hit))
        .collect();
    hits.sort_by(|a, b| b.cmp(a));
    hits.truncate(MAX_RESULTS);
    let partial = scan.partial_note(
        "search a subdirectory with `path` (the pattern is then relative to it) or use a more specific pattern",
        |path| shared.display_abs(path),
    );

    let mut out = String::new();
    if hits.is_empty() && partial.is_some() {
        // A search that stopped early is no evidence of absence.
        let _ = writeln!(
            out,
            "No files matched `{}` among the entries examined. The search stopped before \
             it was done, so matching files may exist beyond what it examined.",
            args.pattern
        );
    } else if hits.is_empty() {
        let _ = writeln!(out, "No files matched `{}`", args.pattern);
    }
    for hit in &hits {
        let _ = writeln!(out, "{}", shared.display_abs(&hit.path));
    }
    if total > MAX_RESULTS {
        let _ = writeln!(
            out,
            "… [results truncated: showing {MAX_RESULTS} of {total} matches; use a narrower pattern or path]"
        );
    }
    if let Some(note) = partial {
        let _ = writeln!(
            out,
            "{note} The files listed are the newest among the entries examined, not of the whole tree."
        );
    }
    Ok(out)
}
