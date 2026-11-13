//! `grep`: gitignore-aware regex search over file contents.

use super::glob::{pruned_walker, search_root, walker};
use super::{
    CallMeta, GREP, MAX_FILE_READ_BYTES, ReadRefusal, Shared, ToolFailure, ToolOutcome,
    looks_binary, read_capped_while, truncate_line,
};
use globset::Glob;
use meerkat_core::ToolDef;
use regex::RegexBuilder;
use serde::Deserialize;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Default maximum number of output lines (entries) when `head_limit` is omitted.
pub(crate) const DEFAULT_HEAD_LIMIT: usize = 250;
/// Hard cap on output size regardless of `head_limit`.
pub(crate) const MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Matched lines longer than this many characters are truncated in `content` mode.
pub(crate) const MAX_LINE_CHARS: usize = 500;

/// What `grep` reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum OutputMode {
    /// One path per matching file.
    #[default]
    FilesWithMatches,
    /// Matching lines as `path:line:text` (context lines as `path-line-text`).
    Content,
    /// Match counts per file as `path:count`.
    Count,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrepArgs {
    #[schemars(description = "Regular expression (Rust `regex` syntax) to search for.")]
    pub pattern: String,
    #[schemars(description = "File or directory to search (default: the project root).")]
    #[serde(default)]
    pub path: Option<String>,
    #[schemars(
        description = "Only search files whose path matches this glob, e.g. `*.rs` or `src/**/*.ts`."
    )]
    #[serde(default)]
    pub glob: Option<String>,
    #[schemars(description = "Match case-insensitively (default false).")]
    #[serde(default)]
    pub case_insensitive: bool,
    #[schemars(
        description = "`files_with_matches` (default): matching file paths; `content`: matching lines as path:line:text; `count`: path:count."
    )]
    #[serde(default)]
    pub output_mode: OutputMode,
    #[schemars(description = "Lines of context before and after each match (content mode only).")]
    #[serde(default)]
    pub context: Option<u32>,
    #[schemars(description = "Maximum number of output lines (default 250).")]
    #[serde(default)]
    pub head_limit: Option<usize>,
}

pub(crate) fn definition() -> ToolDef {
    ToolDef::new(
        GREP,
        format!(
            "Search file contents with a regular expression. Respects .gitignore and skips \
             binary and non-regular files; only the first {} MiB of a file are searched. \
             output_mode selects file paths (default), matching lines, or \
             per-file counts. Output is capped at {DEFAULT_HEAD_LIMIT} lines by default \
             (head_limit) and {} KiB.",
            MAX_FILE_READ_BYTES / (1024 * 1024),
            MAX_OUTPUT_BYTES / 1024
        ),
        meerkat_tools::schema_for::<GrepArgs>(),
    )
}

/// Line- and byte-capped output buffer.
struct Output {
    text: String,
    lines: usize,
    line_limit: usize,
    truncated: bool,
}

impl Output {
    fn new(line_limit: usize) -> Self {
        Self {
            text: String::new(),
            lines: 0,
            line_limit,
            truncated: false,
        }
    }

    /// Append a line; returns `false` once the output is full.
    fn push(&mut self, line: &str) -> bool {
        if self.lines >= self.line_limit || self.text.len() + line.len() + 1 > MAX_OUTPUT_BYTES {
            self.truncated = true;
            return false;
        }
        self.text.push_str(line);
        self.text.push('\n');
        self.lines += 1;
        true
    }
}

/// How many lines a file's matching runs between two budget checks.
const LINES_PER_CHECK: usize = 4096;

/// One walk thread's findings: each file's output lines, by its display path.
#[derive(Default)]
struct Found {
    files: Vec<(String, Vec<String>)>,
}

pub(crate) fn run(shared: &Shared, call: CallMeta, args: GrepArgs) -> ToolOutcome {
    let regex = RegexBuilder::new(&args.pattern)
        .case_insensitive(args.case_insensitive)
        .build()
        .map_err(|err| ToolFailure::recoverable(format!("invalid regex: {err}")))?;
    let filter = args
        .glob
        .as_deref()
        .map(|glob| {
            Glob::new(glob)
                .map(|g| g.compile_matcher())
                .map_err(|err| ToolFailure::recoverable(format!("invalid glob filter: {err}")))
        })
        .transpose()?;
    let root = search_root(shared, GREP, args.path.as_deref())?;
    // An explicit target that is neither a directory nor a regular file (a
    // FIFO, socket or device) is refused rather than silently skipped.
    let root_meta = std::fs::metadata(&root)
        .map_err(|err| ToolFailure::io("search", &shared.display_abs(&root), &err))?;
    if !root_meta.is_dir() && !root_meta.is_file() {
        return Err(ReadRefusal::NotRegular(root_meta.file_type())
            .into_failure("search", &shared.display_abs(&root)));
    }
    let context = args.context.unwrap_or(0) as usize;
    let line_limit = args.head_limit.unwrap_or(DEFAULT_HEAD_LIMIT).max(1);
    // Lines and bytes found so far by every thread: once more than fit the
    // output was found, the walk stops.
    let lines_found = AtomicUsize::new(0);
    let bytes_found = AtomicUsize::new(0);
    let full = AtomicBool::new(false);

    let scan = call.scan(&root);
    let go_on = || scan.go_on();
    // A filter with directories in its literal part confines the matches
    // below them: the walk does not enter the others.
    let walker = match args.glob.as_deref().and_then(super::prune::literal_dirs) {
        Some(dirs) => pruned_walker(
            &root,
            shared.hide_secrets,
            shared.root.secret_roots(),
            move |relative| super::prune::on_the_way(relative, &dirs),
        ),
        None => walker(&root, shared.hide_secrets, shared.root.secret_roots()),
    };
    let found = scan.walk(&walker, |entry, found: &mut Found| {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            return true;
        }
        let path = entry.path();
        if let Some(filter) = &filter {
            let relative = path.strip_prefix(&root).unwrap_or(path);
            let name_matches = path.file_name().is_some_and(|name| filter.is_match(name));
            if !filter.is_match(relative) && !name_matches {
                return true;
            }
        }
        // `read_capped_while` re-checks the path (an entry's type comes
        // from the directory listing) and never reads more than the cap.
        let Ok(Some(read)) = read_capped_while(path, MAX_FILE_READ_BYTES, &go_on) else {
            return true;
        };
        if looks_binary(&read.bytes) {
            return true;
        }
        let text = String::from_utf8_lossy(&read.bytes);
        let display = shared.display_abs(path);
        let (lines, cut) = search_file(
            &text,
            &regex,
            args.output_mode,
            context,
            &display,
            line_limit,
            &go_on,
        );
        if lines.is_empty() {
            return true;
        }
        let count = lines.len();
        let bytes: usize = lines.iter().map(|line| line.len() + 1).sum();
        found.files.push((display, lines));
        let lines_now = lines_found.fetch_add(count, Ordering::Relaxed) + count;
        let bytes_now = bytes_found.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if cut || lines_now > line_limit || bytes_now > MAX_OUTPUT_BYTES {
            full.store(true, Ordering::Relaxed);
            return false;
        }
        true
    });

    let mut files: Vec<(String, Vec<String>)> =
        found.into_iter().flat_map(|found| found.files).collect();
    files.sort_by(|(a, _), (b, _)| a.cmp(b));
    let narrow = if args.glob.is_some() {
        "search a subdirectory with `path` or narrow the `glob` filter"
    } else {
        "search a subdirectory with `path` or pass a `glob` filter such as `*.rs`"
    };
    let partial = scan.partial_note(narrow, |path| shared.display_abs(path));
    let mut out = Output::new(line_limit);
    'files: for (_, lines) in &files {
        for line in lines {
            if !out.push(line) {
                break 'files;
            }
        }
    }

    // A file's own lines were cut at the output's limits.
    out.truncated |= full.load(Ordering::Relaxed);
    let mut text = out.text;
    if files.is_empty() && partial.is_some() {
        // A search that stopped early is no evidence of absence.
        let _ = writeln!(
            text,
            "No matches for `{}` in the files examined. The search stopped before it was \
             done, so matches may exist beyond what it examined.",
            args.pattern
        );
    } else if files.is_empty() {
        let _ = writeln!(text, "No matches for `{}`", args.pattern);
    }
    if out.truncated {
        let order = if full.load(Ordering::Relaxed) {
            " (the search stops once the output is full, so these are sorted by path but \
             not necessarily the first matches by path)"
        } else {
            ""
        };
        let _ = writeln!(
            text,
            "… [output truncated after {} lines{order}; narrow the search or raise head_limit]",
            out.lines
        );
    }
    if let Some(note) = partial {
        let _ = writeln!(text, "{note}");
    }
    Ok(text)
}

/// One file's output lines for `mode`, at most `line_limit` of them, and
/// whether more were cut (the output's line or byte limit).
/// Every mode matches line by line, so `^`, `$` and `(?s).` mean the same
/// thing whichever output was asked for. A count cut short by `go_on`
/// is left out (it would be wrong); found paths and lines are kept.
fn search_file(
    text: &str,
    regex: &regex::Regex,
    mode: OutputMode,
    context: usize,
    display: &str,
    line_limit: usize,
    go_on: &dyn Fn() -> bool,
) -> (Vec<String>, bool) {
    match mode {
        OutputMode::FilesWithMatches => {
            for (n, line) in text.lines().enumerate() {
                if n % LINES_PER_CHECK == LINES_PER_CHECK - 1 && !go_on() {
                    break;
                }
                if regex.is_match(line) {
                    return (vec![display.to_string()], false);
                }
            }
            (Vec::new(), false)
        }
        OutputMode::Count => {
            let mut count = 0usize;
            for (n, line) in text.lines().enumerate() {
                if n % LINES_PER_CHECK == LINES_PER_CHECK - 1 && !go_on() {
                    return (Vec::new(), false);
                }
                count += usize::from(regex.is_match(line));
            }
            if count == 0 {
                (Vec::new(), false)
            } else {
                (vec![format!("{display}:{count}")], false)
            }
        }
        OutputMode::Content => {
            let mut out = Output::new(line_limit);
            content_matches(text, regex, context, display, &mut out, go_on);
            (
                out.text.lines().map(str::to_string).collect(),
                out.truncated,
            )
        }
    }
}
/// Emit matching lines (with `context` surrounding lines) for one file
/// until `out` is full; the matches found before `go_on` says no, when it
/// does.
fn content_matches(
    text: &str,
    regex: &regex::Regex,
    context: usize,
    display: &str,
    out: &mut Output,
    go_on: &dyn Fn() -> bool,
) {
    let lines: Vec<&str> = text.lines().collect();
    let mut matches: Vec<usize> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if idx % LINES_PER_CHECK == LINES_PER_CHECK - 1 && !go_on() {
            break;
        }
        if regex.is_match(line) {
            matches.push(idx);
        }
    }
    if matches.is_empty() {
        return;
    }

    // Merge overlapping context windows into contiguous ranges.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &idx in &matches {
        let start = idx.saturating_sub(context);
        let end = (idx + context).min(lines.len() - 1);
        match ranges.last_mut() {
            Some((_, last_end)) if start <= *last_end + 1 => *last_end = (*last_end).max(end),
            _ => ranges.push((start, end)),
        }
    }

    let mut match_iter = matches.iter().peekable();
    for (n, &(start, end)) in ranges.iter().enumerate() {
        if context > 0 && n > 0 && !out.push("--") {
            return;
        }
        for (idx, line) in lines.iter().enumerate().take(end + 1).skip(start) {
            let is_match = match_iter.peek().is_some_and(|&&m| m == idx);
            if is_match {
                match_iter.next();
            }
            let sep = if is_match { ':' } else { '-' };
            let rendered = format!(
                "{display}{sep}{}{sep}{}",
                idx + 1,
                truncate_line(line, MAX_LINE_CHARS)
            );
            if !out.push(&rendered) {
                return;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn render(text: &str, pattern: &str, context: usize) -> String {
        let regex = regex::Regex::new(pattern).unwrap();
        let mut out = Output::new(100);
        content_matches(text, &regex, context, "f", &mut out, &|| true);
        out.text
    }

    #[test]
    fn content_without_context() {
        assert_eq!(render("a\nb\na\n", "a", 0), "f:1:a\nf:3:a\n");
    }

    #[test]
    fn content_with_context_merges_and_separates() {
        let text = "x\nhit\ny\nz\nw\nv\nhit\n";
        let out = render(text, "hit", 1);
        assert_eq!(out, "f-1-x\nf:2:hit\nf-3-y\n--\nf-6-v\nf:7:hit\n");
    }

    /// The budget is checked inside one file: a count cut short is left
    /// out, matched lines found before the stop are kept, and a file read
    /// stops between chunks.
    #[test]
    fn a_big_file_checks_the_budget_while_it_is_searched() {
        let regex = regex::Regex::new("hit").unwrap();
        let mut text = "hit\n".to_string();
        text.push_str(&"miss\n".repeat(LINES_PER_CHECK * 3));
        let checks = std::cell::Cell::new(0);
        let stop_at_first = || {
            checks.set(checks.get() + 1);
            false
        };
        let (count, _) = search_file(&text, &regex, OutputMode::Count, 0, "f", 10, &stop_at_first);
        assert!(count.is_empty(), "{count:?}");
        assert_eq!(checks.get(), 1, "checked once, then stopped");
        let (lines, _) = search_file(
            &text,
            &regex,
            OutputMode::Content,
            0,
            "f",
            10,
            &stop_at_first,
        );
        assert_eq!(lines, ["f:1:hit"]);
        let (all, _) = search_file(&text, &regex, OutputMode::Count, 0, "f", 10, &|| true);
        assert_eq!(all, ["f:1"]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        std::fs::write(&path, vec![b'x'; 3 * 1024 * 1024]).unwrap();
        let reads = std::cell::Cell::new(0);
        let two_chunks = || {
            reads.set(reads.get() + 1);
            reads.get() <= 2
        };
        assert!(
            super::super::read_capped_while(&path, MAX_FILE_READ_BYTES, &two_chunks)
                .unwrap()
                .is_none()
        );
        let whole = super::super::read_capped_while(&path, MAX_FILE_READ_BYTES, &|| true)
            .unwrap()
            .unwrap();
        assert_eq!(whole.bytes.len(), 3 * 1024 * 1024);
    }

    #[test]
    fn output_caps_lines() {
        let mut out = Output::new(2);
        assert!(out.push("a"));
        assert!(out.push("b"));
        assert!(!out.push("c"));
        assert!(out.truncated);
    }

    #[test]
    fn output_mode_deserializes_snake_case() {
        let mode: OutputMode = serde_json::from_str("\"files_with_matches\"").unwrap();
        assert_eq!(mode, OutputMode::FilesWithMatches);
        let mode: Result<OutputMode, _> = serde_json::from_str("\"content\"");
        assert!(matches!(mode, Ok(OutputMode::Content)));
    }
}
