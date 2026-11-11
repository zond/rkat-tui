//! Project-root path resolution shared by the file tools and the approval policy.
//!
//! Both the tool dispatcher and the tier classifier must agree on what a path
//! means, so this module is the single place that turns a model-supplied string
//! into an absolute path and decides whether it is inside the project and whether
//! it is protected.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

/// Root-relative glob patterns a file tool never writes without asking the user,
/// even inside the project: agent configuration and instruction files, VCS
/// internals and attributes, build/toolchain configuration that runs code, editor
/// task configuration, and secrets.
pub const PROTECTED_PATTERNS: &[&str] = &[
    ".rkat",
    ".rkat/**",
    "**/.rkat",
    "**/.rkat/**",
    ".git",
    ".git/**",
    "**/.git",
    "**/.git/**",
    ".gitattributes",
    ".gitmodules",
    "**/.gitattributes",
    "**/.gitmodules",
    ".claude/**",
    ".env",
    ".env.*",
    ".envrc",
    "**/.env",
    "**/.env.*",
    "**/.envrc",
    "tui.toml",
    ".cargo/**",
    "**/.cargo/**",
    "rust-toolchain",
    "rust-toolchain.toml",
    "**/rust-toolchain*",
    "build.rs",
    "**/build.rs",
    ".vscode/**",
    ".idea/**",
    "AGENTS.md",
    "AGENTS.local.md",
    "**/AGENTS.md",
];

/// Compiled protected patterns. `All` is the fail-closed state used when any
/// pattern fails to compile: every path counts as protected.
#[derive(Debug)]
enum ProtectedSet {
    Globs(GlobSet),
    All,
}

impl ProtectedSet {
    fn build(patterns: &[&str]) -> Self {
        Self::build_with(patterns, false)
    }

    /// The patterns matched ignoring case when `fold_case` (for a project
    /// on a case-insensitive file system).
    fn build_with(patterns: &[&str], fold_case: bool) -> Self {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            match GlobBuilder::new(pattern)
                .case_insensitive(fold_case)
                .build()
            {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(err) => {
                    tracing::error!(%pattern, %err, "invalid protected pattern; protecting every path");
                    return Self::All;
                }
            }
        }
        match builder.build() {
            Ok(set) => Self::Globs(set),
            Err(err) => {
                tracing::error!(%err, "protected patterns failed to compile; protecting every path");
                Self::All
            }
        }
    }

    fn is_match(&self, rel: &Path) -> bool {
        match self {
            Self::Globs(set) => set.is_match(rel),
            Self::All => true,
        }
    }
}

fn protected_set(fold_case: bool) -> &'static ProtectedSet {
    static SET: OnceLock<ProtectedSet> = OnceLock::new();
    static FOLDED: OnceLock<ProtectedSet> = OnceLock::new();
    if fold_case {
        FOLDED.get_or_init(|| ProtectedSet::build_with(PROTECTED_PATTERNS, true))
    } else {
        SET.get_or_init(|| ProtectedSet::build(PROTECTED_PATTERNS))
    }
}

/// Root-relative glob patterns a file tool never writes, whatever the
/// classifier or an allow rule would say: the approval gate's own state
/// and configuration (`.rkat/`: the realm's `config.toml` with its endpoint
/// URLs, the rules, the MCP servers, the sessions) and the repository's VCS
/// internals (`.git`, hooks included). Each is also protected
/// ([`PROTECTED_PATTERNS`]); a shell command is judged by the classifier.
///
/// These cover the project tree only. The same state outside it (the git
/// directory of an enclosing repository or of a worktree, the user config
/// directory `~/.rkat/tui`) is sealed through
/// [`ProjectRoot::with_sealed_roots`]; the workspace's realm directory is a
/// secret root,
/// [`ProjectRoot::with_secret_roots`].
pub const SEALED_PATTERNS: &[&str] = &[
    ".rkat",
    ".rkat/**",
    // A nested `.rkat` too, as the engine's own file tools refuse one at
    // any depth (Meerkat 0.9.12).
    "**/.rkat",
    "**/.rkat/**",
    ".git",
    ".git/**",
    "**/.git",
    "**/.git/**",
];

fn sealed_set(fold_case: bool) -> &'static ProtectedSet {
    static SET: OnceLock<ProtectedSet> = OnceLock::new();
    static FOLDED: OnceLock<ProtectedSet> = OnceLock::new();
    if fold_case {
        FOLDED.get_or_init(|| ProtectedSet::build_with(SEALED_PATTERNS, true))
    } else {
        SET.get_or_init(|| ProtectedSet::build(SEALED_PATTERNS))
    }
}

/// Whether the file system holding `dir` resolves names ignoring case
/// (macOS's default APFS and HFS+, Windows shares): a name of `dir` or of
/// one of its ancestors with its case flipped names the same file. On such
/// a volume `.RKAT/` is the real `.rkat`, so the sealed and protected
/// patterns must match ignoring case there; on a case-sensitive one
/// `.RKAT` is a different, ordinary directory. `false` when no name on the
/// way has a letter or nothing can be read (no evidence of folding).
#[must_use]
pub fn folds_case(dir: &Path) -> bool {
    for path in dir.ancestors() {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let flipped: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect();
        if flipped == name {
            continue;
        }
        let Some(parent) = path.parent() else {
            continue;
        };
        return same_file(path, &parent.join(flipped));
    }
    false
}

/// Whether `a` and `b` are the same existing file (device and inode).
#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// No device and inode to compare: no evidence of folding.
#[cfg(not(unix))]
fn same_file(_a: &Path, _b: &Path) -> bool {
    false
}

/// Whether `path` lies at or below `dir`, comparing each name ignoring
/// case when `fold_case`.
fn below(path: &Path, dir: &Path, fold_case: bool) -> bool {
    if !fold_case {
        return path.starts_with(dir);
    }
    let mut path = path.components();
    dir.components().all(|want| {
        path.next().is_some_and(|have| {
            let (have, want) = (have.as_os_str(), want.as_os_str());
            have == want
                || matches!((have.to_str(), want.to_str()), (Some(h), Some(w)) if h.to_lowercase() == w.to_lowercase())
        })
    })
}

/// Whether `name` is an environment file that typically holds secrets:
/// `.env`, `.env.<anything>` or `.envrc`. The approval classifier's
/// read-only tools never read such files, nor anything below a directory of
/// that name.
pub fn is_secret_file_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|n| n == ".env" || n == ".envrc" || n.starts_with(".env."))
}

/// Errors from resolving a model-supplied path.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// The supplied path was empty.
    #[error("path is empty")]
    Empty,
    /// The supplied path contained a NUL byte.
    #[error("path contains a NUL byte")]
    NulByte,
    /// `~` was used but the home directory is unknown.
    #[error("cannot expand `~`: home directory is unknown")]
    NoHome,
    /// Canonicalization of an existing ancestor failed.
    #[error("failed to resolve {path}: {source}")]
    Io {
        /// Path being resolved.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// The canonical project root. Constructed once at bootstrap.
///
/// Besides the in-project [`SEALED_PATTERNS`], it holds sealed roots:
/// absolute directories a file tool never writes into wherever they lie
/// (see [`Self::with_sealed_roots`] and [`git_dirs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRoot {
    root: PathBuf,
    /// Whether the root's file system resolves names ignoring case
    /// ([`folds_case`]): the patterns and the sealed roots are then matched
    /// ignoring case.
    fold_case: bool,
    /// Canonical (as far as they exist) sealed directories.
    sealed_roots: Vec<PathBuf>,
    /// Canonical (as far as they exist) secret directories: sealed, and
    /// never read either.
    secret_roots: Vec<PathBuf>,
}

impl ProjectRoot {
    /// Canonicalize `path` and use it as the project root, with no sealed
    /// roots beyond [`SEALED_PATTERNS`].
    ///
    /// # Errors
    ///
    /// Returns [`PathError::Io`] when the directory cannot be canonicalized.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, PathError> {
        let path = path.as_ref();
        let canonical = std::fs::canonicalize(path).map_err(|source| PathError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self {
            fold_case: folds_case(&canonical),
            root: canonical,
            sealed_roots: Vec::new(),
            secret_roots: Vec::new(),
        })
    }

    /// Add sealed roots: every path at or below one of `dirs` resolves
    /// [`ResolvedPath::sealed`], inside the project or not. Each directory is
    /// canonicalized as far as it exists, so a symlinked spelling of it, or a
    /// directory created later, still matches. Bootstrap passes the
    /// repository's git directories ([`git_dirs`]), the realm directory and
    /// the user config directory.
    #[must_use]
    pub fn with_sealed_roots(mut self, dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        for dir in dirs {
            let dir = canonicalize_existing_prefix(&dir).unwrap_or(dir);
            if !self.sealed_roots.contains(&dir) {
                self.sealed_roots.push(dir);
            }
        }
        self
    }

    /// The root as if its file system did (`true`) or did not resolve
    /// names ignoring case, whatever [`folds_case`] found: tests of the
    /// case-insensitive matching on a case-sensitive host.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_folded_case(mut self, fold_case: bool) -> Self {
        self.fold_case = fold_case;
        self
    }

    /// Whether `relative` (a path relative to the root) matches
    /// [`SEALED_PATTERNS`], ignoring case on a case-insensitive file
    /// system.
    #[must_use]
    pub fn is_sealed_relative(&self, relative: &Path) -> bool {
        sealed_set(self.fold_case).is_match(relative)
    }

    /// Whether `path` lies at or below one of `dirs`, as this root's file
    /// system compares names.
    #[must_use]
    pub fn below_any(&self, path: &Path, dirs: &[PathBuf]) -> bool {
        dirs.iter().any(|dir| below(path, dir, self.fold_case))
    }

    /// The sealed roots added with [`Self::with_sealed_roots`] (the secret
    /// ones included).
    #[must_use]
    pub fn sealed_roots(&self) -> &[PathBuf] {
        &self.sealed_roots
    }

    /// Add secret roots: sealed ([`Self::with_sealed_roots`]), and every
    /// path at or below one resolves [`ResolvedPath::secret`], which no
    /// file tool reads and no search walks into. The daemon passes the
    /// directories that hold its endpoint file (the attach token).
    #[must_use]
    pub fn with_secret_roots(mut self, dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        for dir in dirs {
            let dir = canonicalize_existing_prefix(&dir).unwrap_or(dir);
            if !self.secret_roots.contains(&dir) {
                self.secret_roots.push(dir.clone());
            }
            if !self.sealed_roots.contains(&dir) {
                self.sealed_roots.push(dir);
            }
        }
        self
    }

    /// The secret roots added with [`Self::with_secret_roots`].
    #[must_use]
    pub fn secret_roots(&self) -> &[PathBuf] {
        &self.secret_roots
    }

    /// The canonical root path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.root
    }

    /// Resolve a model-supplied path against this root.
    ///
    /// Relative paths are joined onto the root (not the process working
    /// directory). `..` is resolved physically, the way the kernel does: the
    /// prefix before it is canonicalized first, so `link/../x` through a
    /// symlinked directory lands next to the link's target. The deepest existing
    /// ancestor of the result is canonicalized so symlinked parents cannot escape
    /// the root unnoticed. The final path does not need to exist.
    ///
    /// # Errors
    ///
    /// Returns a [`PathError`] for empty paths, NUL bytes, an unknown home
    /// directory when `~` is used, or I/O failures during canonicalization.
    pub fn resolve(&self, raw: &str) -> Result<ResolvedPath, PathError> {
        if raw.is_empty() {
            return Err(PathError::Empty);
        }
        if raw.contains('\0') {
            return Err(PathError::NulByte);
        }
        let joined = self.join_literal(raw)?;
        let abs = resolve_physically(&joined)?;
        let relative = self.relative_of(&abs);
        let protected = relative
            .as_deref()
            .is_some_and(|rel| protected_set(self.fold_case).is_match(rel));
        let sealed = relative
            .as_deref()
            .is_some_and(|rel| self.is_sealed_relative(rel))
            || self.below_any(&abs, &self.sealed_roots);
        let secret = self.below_any(&abs, &self.secret_roots);
        Ok(ResolvedPath {
            abs,
            relative,
            protected: protected || sealed,
            sealed,
            secret,
        })
    }

    /// `abs` relative to the root (its names compared ignoring case on a
    /// case-insensitive file system), or `None` outside it.
    fn relative_of(&self, abs: &Path) -> Option<PathBuf> {
        if let Ok(relative) = abs.strip_prefix(&self.root) {
            return Some(relative.to_path_buf());
        }
        if !below(abs, &self.root, self.fold_case) {
            return None;
        }
        Some(
            abs.components()
                .skip(self.root.components().count())
                .collect(),
        )
    }

    /// `raw` with a leading `~` expanded and joined onto the root, *without*
    /// resolving `.`, `..` or symlinks (the file tools inspect the final
    /// component of this path with `lstat`). [`Self::resolve`] starts from it.
    ///
    /// # Errors
    ///
    /// [`PathError::NoHome`] when `raw` starts with `~` and the home
    /// directory is unknown.
    pub fn join_literal(&self, raw: &str) -> Result<PathBuf, PathError> {
        let expanded = expand_tilde(raw)?;
        Ok(if expanded.is_absolute() {
            expanded
        } else {
            self.root.join(expanded)
        })
    }

    /// Display a resolved path relative to the root when possible.
    #[must_use]
    pub fn display_of<'a>(&self, resolved: &'a ResolvedPath) -> std::path::Display<'a> {
        resolved
            .relative
            .as_deref()
            .unwrap_or(&resolved.abs)
            .display()
    }
}

/// The git directories that govern `start`: for `start` and each of its
/// ancestors, the `.git` entry there (a directory, or a file whose
/// `gitdir: <path>` line names one, relative to the file's directory), and
/// that directory's `commondir` (a worktree's or submodule's shared git
/// directory, which holds the hooks and the config). Read from the
/// filesystem only; no `git` subprocess. Paths that do not resolve are
/// skipped.
#[must_use]
pub fn git_dirs(start: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in start.ancestors() {
        let entry = dir.join(".git");
        let Ok(meta) = std::fs::metadata(&entry) else {
            continue;
        };
        let git_dir = if meta.is_dir() {
            entry
        } else {
            match read_pointer(&entry, "gitdir:") {
                Some(target) => dir.join(target),
                None => {
                    // An unreadable pointer is still the repository's.
                    found.push(entry);
                    continue;
                }
            }
        };
        let common = git_dir.join("commondir");
        if common.is_file()
            && let Some(target) = read_pointer(&common, "")
        {
            found.push(git_dir.join(target));
        }
        found.push(git_dir);
    }
    found
}

/// The first line of `file` with `prefix` stripped and trimmed, if any.
fn read_pointer(file: &Path, prefix: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let target = text.lines().next()?.strip_prefix(prefix)?.trim();
    (!target.is_empty()).then(|| PathBuf::from(target))
}

/// A path resolved against a [`ProjectRoot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    /// Absolute path with symlinked ancestors resolved.
    pub abs: PathBuf,
    /// Path relative to the project root, or `None` when it lies outside.
    pub relative: Option<PathBuf>,
    /// Whether the path matches [`PROTECTED_PATTERNS`] (outside the root,
    /// only when it is sealed).
    pub protected: bool,
    /// Whether a file tool never writes the path: inside the root it matches
    /// [`SEALED_PATTERNS`], or it lies at or below one of the root's sealed
    /// roots (the git directories, the realm directory, the user config
    /// directory), inside the project or not. Implies `protected`.
    pub sealed: bool,
    /// Whether no file tool reads the path either: it lies at or below one
    /// of the root's secret roots (the daemon's endpoint directories).
    /// Implies `sealed`.
    pub secret: bool,
}

impl ResolvedPath {
    /// Whether the path lies inside the project root.
    #[must_use]
    pub fn inside(&self) -> bool {
        self.relative.is_some()
    }

    /// Whether the path may be written without classifier review: inside the
    /// root and not protected.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn freely_writable(&self) -> bool {
        self.inside() && !self.protected
    }
}

fn expand_tilde(raw: &str) -> Result<PathBuf, PathError> {
    if raw == "~" {
        return dirs::home_dir().ok_or(PathError::NoHome);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        let home = dirs::home_dir().ok_or(PathError::NoHome)?;
        return Ok(home.join(rest));
    }
    Ok(PathBuf::from(raw))
}

/// Resolve `.` and `..` the way path lookup does: each `..` applies to the
/// canonicalized (symlink-resolved) prefix before it, then the deepest existing
/// ancestor of the result is canonicalized.
fn resolve_physically(path: &Path) -> Result<PathBuf, PathError> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out = canonicalize_existing_prefix(&out)?;
                // Popping the filesystem root is a no-op, matching `/..` == `/`.
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    canonicalize_existing_prefix(&out)
}

/// Canonicalize the deepest existing ancestor of `path` and re-append the rest.
fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf, PathError> {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(canonical) => {
                let mut out = canonical;
                for component in tail.iter().rev() {
                    out.push(component);
                }
                return Ok(out);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = existing.file_name() else {
                    return Err(PathError::Io {
                        path: path.to_path_buf(),
                        source: err,
                    });
                };
                tail.push(name.to_os_string());
                if !existing.pop() {
                    return Err(PathError::Io {
                        path: path.to_path_buf(),
                        source: err,
                    });
                }
            }
            Err(source) => {
                return Err(PathError::Io {
                    path: existing,
                    source,
                });
            }
        }
    }
}

/// How private a file written by [`write_atomic`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// The process's default permissions (umask applies).
    Default,
    /// Owner read/write only (`0600` on Unix): files that may hold secrets
    /// (rules with endpoint keys, notes, the trust record, cost data).
    Private,
    /// Keep an existing target's permission bits; a new file gets `0666`
    /// minus the umask, as `std::fs::write` would create it (the file
    /// tools' writes).
    Preserve,
}

/// Failures of [`write_atomic`] and [`check_no_symlinks`].
#[derive(Debug, thiserror::Error)]
pub enum AtomicWriteError {
    /// The target has no parent directory.
    #[error("{0} has no parent directory")]
    NoParent(PathBuf),
    /// The target or one of its ancestors is a symlink.
    #[error("refusing to write {path}: {component} is a symbolic link")]
    Symlink {
        /// The file to be written.
        path: PathBuf,
        /// The symlinked component.
        component: PathBuf,
    },
    /// The target exists and is not a regular file.
    #[error("refusing to write {0}: it is not a regular file")]
    NotRegular(PathBuf),
    /// An I/O operation failed.
    #[error("{op} {path}: {source}")]
    Io {
        /// What was being done.
        op: &'static str,
        /// On which path.
        path: PathBuf,
        /// The error.
        #[source]
        source: std::io::Error,
    },
}

impl AtomicWriteError {
    fn io(op: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            op,
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Refuse `path` when it, or any existing ancestor of it, is a symbolic link
/// (each component checked with `symlink_metadata`). Missing components are
/// fine: they will be created as real directories.
///
/// # Errors
///
/// [`AtomicWriteError::Symlink`] naming the first symlinked component, or
/// [`AtomicWriteError::Io`] when a component cannot be inspected.
pub fn check_no_symlinks(path: &Path) -> Result<(), AtomicWriteError> {
    check_no_symlinks_from(Path::new(""), path, path)
}

/// Write `bytes` to `path` atomically: a temporary file in the same
/// directory, flushed and synced, then renamed over the target
/// (`tempfile::NamedTempFile::new_in(parent)?.persist(path)`). Missing parent
/// directories are created. A target that is, or is reached through, a
/// symlink is refused (only `root_trusted` — the part of `path` the caller
/// itself chose, such as the user's config directory — is not checked), as
/// is an existing target that is not a regular file. `mode` sets the file's
/// permissions before the rename. The target's inode is never written:
/// a hard link to the old content keeps it.
///
/// # Errors
///
/// [`AtomicWriteError`] when the target is refused or an I/O step fails.
pub fn write_atomic(
    path: &Path,
    bytes: &[u8],
    mode: FileMode,
    root_trusted: Option<&Path>,
) -> Result<(), AtomicWriteError> {
    use std::io::Write as _;

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| AtomicWriteError::NoParent(path.to_path_buf()))?;
    match root_trusted.and_then(|root| Some((root, path.strip_prefix(root).ok()?))) {
        Some((root, rel)) => check_no_symlinks_from(root, rel, path)?,
        None => check_no_symlinks(path)?,
    }
    let existing = match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(AtomicWriteError::NotRegular(path.to_path_buf()));
        }
        Ok(meta) => Some(meta),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(AtomicWriteError::io("inspect", path, err)),
    };
    std::fs::create_dir_all(parent)
        .map_err(|e| AtomicWriteError::io("create directory", parent, e))?;
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    if mode == FileMode::Preserve && existing.is_none() {
        // `open(2)` applies the umask to this mode, as for a plain create.
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let mut temp = builder
        .tempfile_in(parent)
        .map_err(|e| AtomicWriteError::io("create a temporary file in", parent, e))?;
    let exact = match mode {
        FileMode::Private => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                Some(std::fs::Permissions::from_mode(0o600))
            }
            #[cfg(not(unix))]
            {
                None
            }
        }
        FileMode::Preserve => existing.map(|meta| meta.permissions()),
        FileMode::Default => None,
    };
    if let Some(permissions) = exact {
        temp.as_file()
            .set_permissions(permissions)
            .map_err(|e| AtomicWriteError::io("set permissions of", temp.path(), e))?;
    }
    temp.write_all(bytes)
        .map_err(|e| AtomicWriteError::io("write", temp.path(), e))?;
    temp.as_file()
        .sync_all()
        .map_err(|e| AtomicWriteError::io("sync", temp.path(), e))?;
    temp.persist(path)
        .map_err(|e| AtomicWriteError::io("replace", path, e.error))?;
    Ok(())
}

/// [`check_no_symlinks`] for the components of `rel` below `base`.
fn check_no_symlinks_from(base: &Path, rel: &Path, path: &Path) -> Result<(), AtomicWriteError> {
    let mut current = base.to_path_buf();
    for component in rel.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::RootDir | Component::Prefix(_)) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(AtomicWriteError::Symlink {
                    path: path.to_path_buf(),
                    component: current,
                });
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(AtomicWriteError::io("inspect", &current, err)),
        }
    }
    Ok(())
}

/// This machine's host name.
#[must_use]
pub fn local_host() -> Option<String> {
    #[cfg(unix)]
    {
        nix::unistd::gethostname()
            .ok()
            .and_then(|name| name.into_string().ok())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn root() -> (tempfile::TempDir, ProjectRoot) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let root = ProjectRoot::new(dir.path()).unwrap();
        (dir, root)
    }

    /// The file tools' literal path and the resolver share one tilde
    /// expansion and one join; the literal path keeps `..` and symlinks.
    #[test]
    fn join_literal_expands_tilde_and_keeps_the_path_literal() {
        let (_dir, root) = root();
        let home = dirs::home_dir().unwrap();
        assert_eq!(root.join_literal("~").unwrap(), home);
        assert_eq!(
            root.join_literal("~/notes.md").unwrap(),
            home.join("notes.md")
        );
        assert_eq!(
            root.join_literal("src/../src/main.rs").unwrap(),
            root.as_path().join("src/../src/main.rs")
        );
        assert_eq!(
            root.join_literal("/etc/hosts").unwrap(),
            PathBuf::from("/etc/hosts")
        );
        assert_eq!(
            root.join_literal("~other/x").unwrap(),
            root.as_path().join("~other/x")
        );
    }

    #[test]
    fn relative_paths_join_onto_root() {
        let (_dir, root) = root();
        let r = root.resolve("src/main.rs").unwrap();
        assert!(r.inside());
        assert_eq!(r.relative.as_deref(), Some(Path::new("src/main.rs")));
        assert!(!r.protected);
        assert!(r.freely_writable());
    }

    #[test]
    fn nonexistent_file_still_resolves_inside() {
        let (_dir, root) = root();
        let r = root.resolve("src/new/deep/file.rs").unwrap();
        assert!(r.inside());
        assert_eq!(
            r.relative.as_deref(),
            Some(Path::new("src/new/deep/file.rs"))
        );
    }

    #[test]
    fn dotdot_escape_is_outside() {
        let (_dir, root) = root();
        let r = root.resolve("../outside.txt").unwrap();
        assert!(!r.inside());
        assert!(!r.protected);
        assert!(!r.freely_writable());
    }

    #[test]
    fn dotdot_within_root_normalizes() {
        let (_dir, root) = root();
        let r = root.resolve("src/../src/./main.rs").unwrap();
        assert_eq!(r.relative.as_deref(), Some(Path::new("src/main.rs")));
    }

    #[test]
    fn absolute_path_inside_root() {
        let (dir, root) = root();
        let abs = dir.path().join("src/main.rs");
        let r = root.resolve(abs.to_str().unwrap()).unwrap();
        assert!(r.inside());
    }

    #[test]
    fn sealed_paths_are_the_gate_state_and_vcs_internals_and_imply_protected() {
        let (_dir, root) = root();
        for p in [
            // `.rkat/**` alone does not match the directory entry itself.
            ".rkat",
            ".rkat/config.toml",
            ".rkat/tui/tui.toml",
            "a/b/.rkat",
            "a/b/.rkat/mcp.toml",
            ".git",
            ".git/hooks/pre-commit",
            "vendor/.git/HEAD",
        ] {
            let r = root.resolve(p).unwrap();
            assert!(r.sealed, "{p}");
            assert!(r.protected, "{p}");
        }
        for p in [
            ".env",
            "build.rs",
            "AGENTS.md",
            "src/main.rs",
            "a/.rkatx/b",
            "a/b.rkat",
        ] {
            assert!(!root.resolve(p).unwrap().sealed, "{p}");
        }
        for pattern in SEALED_PATTERNS {
            assert!(PROTECTED_PATTERNS.contains(pattern), "{pattern}");
        }
    }

    /// Sealed roots outside the project: an enclosing repository's git dir,
    /// a worktree's git dir and its common dir, a realm and a config dir.
    #[test]
    fn sealed_roots_seal_state_outside_the_project() {
        let base = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(base.path()).unwrap();
        // The workspace is a subdirectory of a repository.
        let repo = base.join("repo");
        let ws = repo.join("sub/ws");
        std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        let root = ProjectRoot::new(&ws).unwrap();
        assert!(!root.resolve("../../.git/hooks/pre-commit").unwrap().sealed);
        let root = root.with_sealed_roots(git_dirs(&ws));
        let r = root.resolve("../../.git/hooks/pre-commit").unwrap();
        assert!(r.sealed && r.protected && !r.inside(), "{r:?}");
        assert!(!root.resolve("../../README.md").unwrap().sealed);

        // A worktree: `.git` is a file naming its git dir, whose `commondir`
        // names the main repository's git dir (hooks, config).
        let main_git = base.join("main/.git");
        let wt_git = main_git.join("worktrees/x");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let wt = base.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        let root = ProjectRoot::new(&wt)
            .unwrap()
            .with_sealed_roots(git_dirs(&wt));
        for p in [
            main_git.join("hooks/pre-commit"),
            main_git.join("config"),
            wt_git.join("HEAD"),
        ] {
            assert!(root.resolve(p.to_str().unwrap()).unwrap().sealed, "{p:?}");
        }

        // A realm and a config dir outside the workspace, sealed even before
        // they exist.
        let realm = base.join("state/realm-x");
        let config = base.join("home/.rkat/tui");
        let root = ProjectRoot::new(&ws)
            .unwrap()
            .with_sealed_roots([realm.clone(), config.clone()]);
        for p in [realm.join("config.toml"), config.join("rules.toml")] {
            assert!(root.resolve(p.to_str().unwrap()).unwrap().sealed, "{p:?}");
        }
        assert!(
            !root
                .resolve(base.join("state/other").to_str().unwrap())
                .unwrap()
                .sealed
        );
    }

    #[test]
    fn protected_patterns_match() {
        let (_dir, root) = root();
        for p in [
            ".rkat/config.toml",
            ".git/HEAD",
            ".claude/settings.json",
            ".env",
            ".env.local",
            "svc/.env",
            ".envrc",
            "tui.toml",
            ".cargo/config.toml",
            "crates/x/.cargo/config.toml",
            "rust-toolchain",
            "rust-toolchain.toml",
            "sub/rust-toolchain.toml",
            "vendor/dep/.git/config",
            ".gitattributes",
            ".gitmodules",
            "sub/.gitattributes",
            "sub/.gitmodules",
            ".vscode/tasks.json",
            ".idea/workspace.xml",
            "build.rs",
            "crates/x/build.rs",
            "AGENTS.md",
            "AGENTS.local.md",
            "docs/AGENTS.md",
            // The file form of `.git` (a worktree or submodule pointer).
            ".git",
            "vendor/dep/.git",
        ] {
            let r = root.resolve(p).unwrap();
            assert!(r.inside(), "{p} should be inside");
            assert!(r.protected, "{p} should be protected");
            assert!(!r.freely_writable());
        }
        for p in [
            "src/env.rs",
            "environment.md",
            "gitignore",
            ".github/ci.yml",
            "src/build.rs.bak",
            "src/builder.rs",
            "cargo/notes.md",
            "AGENTS.txt",
        ] {
            let r = root.resolve(p).unwrap();
            assert!(!r.protected, "{p} should not be protected");
        }
    }

    #[test]
    fn git_file_form_is_not_tier_two_writable() {
        let (dir, root) = root();
        // A worktree's `.git` is a file pointing at the git dir: rewriting it
        // redirects every git command.
        std::fs::write(dir.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        let r = root.resolve(".git").unwrap();
        assert!(r.protected && !r.freely_writable());
        assert!(root.resolve("sub/.git").unwrap().protected);
        assert!(!root.resolve("src/.github").unwrap().protected);
    }

    #[test]
    fn atomic_write_replaces_and_sets_private_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/secret.toml");
        write_atomic(&path, b"one", FileMode::Private, Some(dir.path())).unwrap();
        write_atomic(&path, b"two", FileMode::Private, Some(dir.path())).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // No temporary files are left behind.
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["secret.toml"]);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_never_writes_a_shared_inode() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let alias = dir.path().join("alias");
        std::fs::hard_link(&victim, &alias).unwrap();
        write_atomic(&alias, b"new", FileMode::Preserve, Some(dir.path())).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&alias).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_refuses_symlinked_targets_and_parents() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let err = write_atomic(&link, b"x", FileMode::Default, Some(dir.path())).unwrap_err();
        assert!(matches!(err, AtomicWriteError::Symlink { .. }), "{err}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");

        std::os::unix::fs::symlink(outside.path(), dir.path().join("sub")).unwrap();
        let through = dir.path().join("sub/new.toml");
        let err = write_atomic(&through, b"x", FileMode::Default, Some(dir.path())).unwrap_err();
        assert!(matches!(err, AtomicWriteError::Symlink { .. }), "{err}");
        assert!(!outside.path().join("new.toml").exists());

        // A symlink above the trusted root is the caller's choice.
        let via = dir.path().join("via");
        std::os::unix::fs::symlink(outside.path(), &via).unwrap();
        write_atomic(&via.join("ok.toml"), b"x", FileMode::Default, Some(&via)).unwrap();
        assert!(check_no_symlinks(&via.join("ok.toml")).is_err());
    }

    #[test]
    fn empty_and_nul_are_errors() {
        let (_dir, root) = root();
        assert!(matches!(root.resolve(""), Err(PathError::Empty)));
        assert!(matches!(root.resolve("a\0b"), Err(PathError::NulByte)));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directory_escaping_root_is_outside() {
        let (dir, root) = root();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let r = root.resolve("link/secret.txt").unwrap();
        assert!(!r.inside(), "symlink escape must resolve outside the root");
        let r2 = root.resolve("link/not-yet-created.txt").unwrap();
        assert!(!r2.inside());
    }

    #[test]
    fn protected_set_fails_closed() {
        let bad = ProtectedSet::build(&["src/**", "[unclosed"]);
        assert!(matches!(bad, ProtectedSet::All));
        assert!(bad.is_match(Path::new("anything.txt")));
        let good = ProtectedSet::build(&["src/**"]);
        assert!(good.is_match(Path::new("src/main.rs")));
        assert!(!good.is_match(Path::new("README.md")));
    }

    /// On a case-insensitive file system (macOS's default) `.RKAT/` is the
    /// real `.rkat`: the seal, the protected patterns and the sealed roots
    /// match ignoring case there. Linux file systems are case-sensitive, so
    /// the folding answer is injected here; `folds_case` itself is pinned
    /// to say no for a case-sensitive directory.
    #[test]
    fn a_case_insensitive_volume_seals_every_spelling() {
        let (_dir, root) = root();
        let cfg = root.as_path().join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        let folded = root.clone().with_sealed_roots([cfg]).with_folded_case(true);
        for path in [
            ".RKAT/tui.toml",
            ".Rkat",
            "sub/.rKaT/mcp.toml",
            ".GIT/config",
        ] {
            let resolved = folded.resolve(path).unwrap();
            assert!(
                resolved.sealed && resolved.protected,
                "{path}: {resolved:?}"
            );
        }
        for path in [".ENV", "AGENTS.MD", "Build.rs"] {
            assert!(folded.resolve(path).unwrap().protected, "{path}");
        }
        assert!(folded.resolve("CFG/tui.toml").unwrap().sealed);
        // An absolute spelling of the root in another case is inside it.
        let upper = folded.as_path().to_string_lossy().to_uppercase();
        let resolved = folded.resolve(&format!("{upper}/.rkat/x")).unwrap();
        assert!(resolved.inside() && resolved.sealed, "{resolved:?}");
        // On a case-sensitive volume `.RKAT` is an ordinary directory.
        let sensitive = root.with_folded_case(false);
        let plain = sensitive.resolve(".RKAT/tui.toml").unwrap();
        assert!(!plain.sealed && !plain.protected, "{plain:?}");
        assert!(sensitive.resolve(".rkat/tui.toml").unwrap().sealed);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_case_sensitive_directory_does_not_fold_case() {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().join("Workspace");
        std::fs::create_dir_all(&named).unwrap();
        assert!(!folds_case(&named));
        assert!(
            !ProjectRoot::new(&named)
                .unwrap()
                .is_sealed_relative(Path::new(".RKAT"))
        );
        assert!(
            ProjectRoot::new(&named)
                .unwrap()
                .is_sealed_relative(Path::new(".rkat"))
        );
    }

    #[test]
    fn all_builtin_protected_patterns_compile() {
        assert!(matches!(protected_set(false), ProtectedSet::Globs(_)));
        assert!(matches!(protected_set(true), ProtectedSet::Globs(_)));
    }

    #[cfg(unix)]
    #[test]
    fn dotdot_through_symlinked_directory_is_physical() {
        let (dir, root) = root();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("inner")).unwrap();
        std::fs::write(outside.path().join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(outside.path().join("inner"), dir.path().join("lnk")).unwrap();
        let r = root.resolve("lnk/../secret").unwrap();
        assert!(!r.inside(), "`..` must apply to the symlink target");
        assert_eq!(r.abs, outside.path().canonicalize().unwrap().join("secret"));
        let r2 = root.resolve("lnk/../not-yet-created").unwrap();
        assert!(!r2.inside());
    }

    #[test]
    fn dotdot_through_missing_directory_is_lexical() {
        let (_dir, root) = root();
        let r = root.resolve("missing/../src/main.rs").unwrap();
        assert_eq!(r.relative.as_deref(), Some(Path::new("src/main.rs")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directory_inside_root_stays_inside() {
        let (dir, root) = root();
        std::os::unix::fs::symlink(dir.path().join("src"), dir.path().join("alias")).unwrap();
        let r = root.resolve("alias/main.rs").unwrap();
        assert!(r.inside());
        assert_eq!(r.relative.as_deref(), Some(Path::new("src/main.rs")));
    }
}
