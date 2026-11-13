//! Which directories a glob pattern can still match below.
//!
//! A `glob` walk descends into every directory, though a pattern such as
//! `*-pnpm*/bin/pnpm` can only match three levels deep, under a first-level
//! directory matching `*-pnpm*`. [`Prune`] compiles a pattern into one
//! matcher per `/`-separated segment (`**` matching any number of them) and
//! answers, for a directory's path relative to the search root, whether any
//! path below it can match: the walk skips the directories where none can.
//! A file is still matched against the whole pattern, so the answer is the
//! same as an unpruned walk's; only fewer entries are examined. A pattern
//! whose segments cannot be told apart safely (an alternation `{…}`, a
//! character class or escape that could hold `/`) is not pruned.

use globset::{GlobBuilder, GlobMatcher};
use std::path::Path;

/// One segment of a pattern.
#[derive(Debug, Clone)]
enum Segment {
    /// `**`: any number of names, none included.
    AnyDepth,
    /// One name matching this glob.
    Name(GlobMatcher),
}

/// A glob pattern as per-depth segment matchers ([`Prune::compile`]).
#[derive(Debug, Clone)]
pub(crate) struct Prune {
    segments: Vec<Segment>,
}

impl Prune {
    /// The segments of `pattern`, or `None` when it cannot be pruned
    /// safely (see the module docs) or a segment does not compile.
    pub(crate) fn compile(pattern: &str) -> Option<Self> {
        if pattern.is_empty() || pattern.contains(['{', '}', '[', ']', '\\']) {
            return None;
        }
        let mut segments = Vec::new();
        for part in pattern.split('/') {
            if part.is_empty() {
                return None;
            }
            if part == "**" {
                // `a/**/**/b` is `a/**/b`.
                if !matches!(segments.last(), Some(Segment::AnyDepth)) {
                    segments.push(Segment::AnyDepth);
                }
                continue;
            }
            let glob = GlobBuilder::new(part)
                .literal_separator(true)
                .build()
                .ok()?
                .compile_matcher();
            segments.push(Segment::Name(glob));
        }
        Some(Self { segments })
    }

    /// The positions in the pattern reachable from `states` with a `**`
    /// matching nothing.
    fn close(&self, states: &mut Vec<usize>) {
        let mut i = 0;
        while i < states.len() {
            let at = states[i];
            if matches!(self.segments.get(at), Some(Segment::AnyDepth))
                && !states.contains(&(at + 1))
            {
                states.push(at + 1);
            }
            i += 1;
        }
    }

    /// Whether a path strictly below the directory `relative` (relative to
    /// the search root, the root itself empty) can match the pattern.
    pub(crate) fn may_match_below(&self, relative: &Path) -> bool {
        let mut states = vec![0];
        self.close(&mut states);
        for name in relative.components() {
            let name = name.as_os_str();
            let mut next = Vec::new();
            for &at in &states {
                let to = match self.segments.get(at) {
                    Some(Segment::AnyDepth) => at,
                    Some(Segment::Name(glob)) if glob.is_match(name) => at + 1,
                    _ => continue,
                };
                if !next.contains(&to) {
                    next.push(to);
                }
            }
            self.close(&mut next);
            if next.is_empty() {
                return false;
            }
            states = next;
        }
        // A deeper path needs a segment still to match.
        states.iter().any(|&at| at < self.segments.len())
    }
}

/// The directories `grep`'s `glob` filter confines a match to: the whole
/// names before its first wildcard (`src/ui/*.rs` → `src/ui`), when the
/// filter holds a `/` there. The filter is matched with `*` crossing `/`
/// against the path relative to the search root, or against the file's
/// name alone; a name holds no `/`, so with one in its literal part only
/// the relative path can match, and only below that prefix. `None` when
/// the filter gives no such prefix, or has syntax that is not split
/// safely (as [`Prune::compile`]).
pub(crate) fn literal_dirs(filter: &str) -> Option<std::path::PathBuf> {
    if filter.contains(['{', '}', '[', ']', '\\']) {
        return None;
    }
    let literal = &filter[..filter.find(['*', '?']).unwrap_or(filter.len())];
    let cut = literal.rfind('/')?;
    let dirs = &literal[..cut];
    if dirs.is_empty()
        || dirs
            .split('/')
            .any(|name| name.is_empty() || name == "." || name == "..")
    {
        return None;
    }
    Some(std::path::PathBuf::from(dirs))
}

/// Whether the directory `relative` can hold a path below `dirs` (it is on
/// the way to it, or inside it).
pub(crate) fn on_the_way(relative: &Path, dirs: &Path) -> bool {
    relative.starts_with(dirs) || dirs.starts_with(relative)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use globset::GlobBuilder;
    use std::path::PathBuf;

    /// A small deterministic generator (xorshift), so a failure repeats.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[usize::try_from(self.next() % items.len() as u64).unwrap()]
        }
    }

    const NAMES: &[&str] = &[
        "a", "b", "ab", "x-pnpm-1", "bin", "pnpm", ".h", "a.rs", "lib",
    ];
    const SEGMENTS: &[&str] = &[
        "a", "b", "*", "**", "?", "a*", "*b", "*-pnpm*", "bin", "pnpm", "*.rs", "?b", ".h",
    ];

    /// For every generated pattern and path, a path the pattern matches
    /// has no ancestor directory the pruning skips: a pruned walk finds
    /// exactly what an unpruned one finds.
    #[test]
    fn pruning_never_skips_a_directory_holding_a_match() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut paths: Vec<PathBuf> = Vec::new();
        for _ in 0..400 {
            let depth = 1 + rng.next() % 5;
            let mut path = PathBuf::new();
            for _ in 0..depth {
                path.push(rng.pick(NAMES));
            }
            paths.push(path);
        }
        let mut checked = 0;
        for _ in 0..600 {
            let segments = 1 + rng.next() % 4;
            let pattern: Vec<&str> = (0..segments).map(|_| rng.pick(SEGMENTS)).collect();
            let pattern = pattern.join("/");
            let Ok(glob) = GlobBuilder::new(&pattern).literal_separator(true).build() else {
                continue;
            };
            let full = glob.compile_matcher();
            let prune = Prune::compile(&pattern).unwrap();
            for path in &paths {
                if !full.is_match(path) {
                    continue;
                }
                checked += 1;
                for dir in path.ancestors().skip(1) {
                    assert!(
                        prune.may_match_below(dir),
                        "`{pattern}` matches {} but prunes {}",
                        path.display(),
                        dir.display()
                    );
                }
            }
        }
        assert!(checked > 100, "only {checked} matches checked");
    }

    #[test]
    fn a_pattern_without_double_star_stops_at_its_depth_and_names() {
        let prune = Prune::compile("*-pnpm*/bin/pnpm").unwrap();
        assert!(prune.may_match_below(Path::new("")));
        assert!(prune.may_match_below(Path::new("abc-pnpm-9")));
        assert!(prune.may_match_below(Path::new("abc-pnpm-9/bin")));
        assert!(!prune.may_match_below(Path::new("abc-pnpm-9/bin/pnpm")));
        assert!(!prune.may_match_below(Path::new("abc-source")));
        assert!(!prune.may_match_below(Path::new("abc-pnpm-9/lib")));
        let deep = Prune::compile("src/**/*.rs").unwrap();
        assert!(deep.may_match_below(Path::new("src/a/b/c")));
        assert!(!deep.may_match_below(Path::new("target")));
        let any = Prune::compile("**/*.rs").unwrap();
        assert!(any.may_match_below(Path::new("x/y/z")));
        assert!(Prune::compile("{a,b}/c").is_none());
        assert!(Prune::compile("a/[bc]/d").is_none());
    }

    #[test]
    fn a_filter_s_literal_directories() {
        assert_eq!(literal_dirs("src/ui/*.rs"), Some(PathBuf::from("src/ui")));
        assert_eq!(literal_dirs("src/main.rs"), Some(PathBuf::from("src")));
        assert_eq!(literal_dirs("*.rs"), None);
        assert_eq!(literal_dirs("src*/x"), None);
        assert_eq!(literal_dirs("{a,b}/x"), None);
        assert!(on_the_way(Path::new("src"), Path::new("src/ui")));
        assert!(on_the_way(Path::new("src/ui/x"), Path::new("src/ui")));
        assert!(!on_the_way(Path::new("target"), Path::new("src/ui")));
    }
}
