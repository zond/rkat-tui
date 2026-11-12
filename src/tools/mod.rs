//! In-crate file tools: `read_file`, `write_file`, `edit_file`, `glob`, `grep`,
//! and the watch tools `watch_paths` and `unwatch` ([`WatchToolDispatcher`]).
//!
//! [`FileToolDispatcher`] implements [`AgentToolDispatcher`] and is injected
//! into sessions as an external tool source. It is deliberately policy-free:
//! out-of-project paths are not refused here (the approval layer decides), and
//! the only hard refusals are writing through a symlink and writing a file
//! with more than one hard link. Writes replace the file (a temporary file
//! renamed over it), so a shared inode is never written.
//!
//! Every model-supplied path goes through [`ProjectRoot::resolve`] so the tools
//! and the approval policy agree on what a path means.
//!
//! Failure convention: problems the model can recover from (file not found,
//! no match, stale file, invalid pattern) become `Ok(ToolResult { is_error:
//! true, .. })`; malformed arguments and internal failures become
//! `Err(ToolError)`.

mod diff;
mod edit;
mod glob;
mod grep;
mod read;
mod read_only;
mod scan;
mod watch;
mod write;

pub use diff::FileDiff;
pub(crate) use glob::mention_walker;

pub use read_only::ReadOnlyProjectTools;
pub use scan::SearchLimits;
pub(crate) use scan::run_cancellable;
#[cfg(any(test, feature = "test-support"))]
pub use scan::{DEFAULT_SEARCH_BUDGET_SECS, DEFAULT_SEARCH_MAX_ENTRIES, WalkProbe};
pub use watch::{UNWATCH, WATCH_PATHS, WATCH_TOOLS_SOURCE, WatchHost, WatchToolDispatcher};

/// The catalog support of a dispatcher whose tool list is fixed when it is
/// built: its `tool_catalog` (the default, mirroring `tools`) is exact.
///
/// A gateway's catalog is exact only when every dispatcher in it says so,
/// and an inexact one is rebuilt from the tool definitions alone, without
/// each tool's execution contract. Composed beside the engine's builtins,
/// a dispatcher that left this at the default took the shell's `Detached`
/// mode out of the agent's catalog, and every `shell` with `background`
/// was refused (`tests/shell_jobs.rs`).
#[must_use]
pub fn fixed_catalog() -> meerkat_core::ToolCatalogCapabilities {
    meerkat_core::ToolCatalogCapabilities {
        exact_catalog: true,
        may_require_catalog_control_plane: false,
    }
}

use crate::paths::{PathError, ProjectRoot, ResolvedPath};
use async_trait::async_trait;
use meerkat_core::ops::ToolDispatchOutcome;
use meerkat_core::{
    AgentToolDispatcher, SessionId, ToolCallView, ToolDef, ToolDispatchContext, ToolError,
    ToolMutationClass, ToolResult,
};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::mpsc;

/// Tool name for [`read`].
pub const READ_FILE: &str = "read_file";

/// Provenance source id of the file tools. Stable across builds: a helper's
/// inherited tool ceiling (a mob `delegate` with `inherit_parent` tooling)
/// matches its parent's tools by provenance, so every build tags them alike.
pub const FILE_TOOLS_SOURCE: &str = "rkat-tui.files";

/// Provenance of a TUI-owned tool: a host callback surface named `source`.
#[must_use]
pub fn provenance(source: &str) -> meerkat_core::ToolProvenance {
    meerkat_core::ToolProvenance::new(
        meerkat_core::ToolSourceKind::Callback,
        meerkat_core::ToolSourceId::new(source),
    )
}
/// Tool name for [`write`](mod@write).
pub const WRITE_FILE: &str = "write_file";
/// Tool name for [`edit`].
pub const EDIT_FILE: &str = "edit_file";
/// Tool name for [`glob`].
pub const GLOB: &str = "glob";
/// Tool name for [`grep`].
pub const GREP: &str = "grep";

/// Names of every tool exposed by [`FileToolDispatcher`].
#[cfg(any(test, feature = "test-support"))]
pub const FILE_TOOL_NAMES: &[&str] = &[READ_FILE, WRITE_FILE, EDIT_FILE, GLOB, GREP];

/// Number of leading bytes inspected for a NUL byte when detecting binary files.
pub(crate) const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// How many read-before-write records the file tools keep, for every
/// session together.
pub const MAX_READ_RECORDS: usize = 4096;

/// Put `key` -> `value` into `map` as its newest entry and drop the oldest
/// past `cap` (a bounded record kept in insertion order).
pub(crate) fn keep_newest<K: std::hash::Hash + Eq, V>(
    map: &mut indexmap::IndexMap<K, V>,
    key: K,
    value: V,
    cap: usize,
) {
    map.shift_remove(&key);
    map.insert(key, value);
    while map.len() > cap {
        map.shift_remove_index(0);
    }
}

/// Most bytes any file tool reads from one file. `read_file` and `grep`
/// see at most this prefix; `write_file`/`edit_file` refuse to rewrite a
/// larger file (rewriting a prefix would drop its tail).
pub const MAX_FILE_READ_BYTES: u64 = 16 * 1024 * 1024;

/// A file's bytes read through a cap.
pub(crate) struct CappedRead {
    pub(crate) bytes: Vec<u8>,
    /// The file held more than the cap; `bytes` is its first `cap` bytes.
    pub(crate) truncated: bool,
}

/// Why a path could not be read as a regular file.
#[derive(Debug)]
pub(crate) enum ReadRefusal {
    /// Not a regular file (directory, FIFO, socket, device, ...).
    NotRegular(std::fs::FileType),
    Io(io::Error),
}

impl From<io::Error> for ReadRefusal {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl ReadRefusal {
    /// Describe the refusal in model-facing terms.
    pub(crate) fn into_failure(self, action: &str, display: &str) -> ToolFailure {
        match self {
            Self::NotRegular(kind) if kind.is_dir() => {
                ToolFailure::recoverable(format!("{display} is a directory, not a file"))
            }
            Self::NotRegular(kind) => ToolFailure::recoverable(format!(
                "{display} is not a regular file ({}); only regular files can be read",
                file_kind(kind)
            )),
            Self::Io(err) => ToolFailure::io(action, display, &err),
        }
    }
}

/// A short name for a non-regular file type.
pub(crate) fn file_kind(kind: std::fs::FileType) -> &'static str {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        if kind.is_fifo() {
            return "a named pipe";
        }
        if kind.is_socket() {
            return "a socket";
        }
        if kind.is_char_device() || kind.is_block_device() {
            return "a device";
        }
    }
    if kind.is_dir() {
        "a directory"
    } else if kind.is_symlink() {
        "a symlink"
    } else {
        "a special file"
    }
}

/// Read at most `cap` bytes of the regular file at `path`
/// ([`open_regular`]). The read goes through [`io::Read::take`], so a file
/// growing after the check still yields at most `cap` bytes.
pub(crate) fn read_capped(path: &Path, cap: u64) -> Result<CappedRead, ReadRefusal> {
    let file = open_regular(path)?;
    let mut bytes = Vec::with_capacity(initial_capacity(&file, cap));
    file.take(cap.saturating_add(1)).read_to_end(&mut bytes)?;
    Ok(CappedRead::of(bytes, cap))
}

/// As [`read_capped`], in chunks of [`READ_CHUNK_BYTES`], asking `go_on`
/// before each: `Ok(None)` when it said no (a search out of time or
/// cancelled), so a slow file cannot hold a search past its budget.
pub(crate) fn read_capped_while(
    path: &Path,
    cap: u64,
    go_on: &dyn Fn() -> bool,
) -> Result<Option<CappedRead>, ReadRefusal> {
    let file = open_regular(path)?;
    let mut bytes = Vec::with_capacity(initial_capacity(&file, cap));
    let mut rest = file.take(cap.saturating_add(1));
    loop {
        if !go_on() {
            return Ok(None);
        }
        let read = (&mut rest).take(READ_CHUNK_BYTES).read_to_end(&mut bytes)?;
        if read == 0 {
            break;
        }
    }
    Ok(Some(CappedRead::of(bytes, cap)))
}

/// How much [`read_capped_while`] reads between two checks.
const READ_CHUNK_BYTES: u64 = 1024 * 1024;

/// The regular file at `path`, opened. The path is checked with `metadata`
/// before it is opened (opening a FIFO blocks until a writer appears, and
/// a device may never end), and the open handle is checked again (the
/// path may have been swapped in between).
fn open_regular(path: &Path) -> Result<std::fs::File, ReadRefusal> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Err(ReadRefusal::NotRegular(meta.file_type()));
    }
    let file = std::fs::File::open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() {
        return Err(ReadRefusal::NotRegular(opened.file_type()));
    }
    Ok(file)
}

fn initial_capacity(file: &std::fs::File, cap: u64) -> usize {
    let len = file.metadata().map_or(0, |meta| meta.len());
    usize::try_from(len.min(cap)).unwrap_or(0)
}

impl CappedRead {
    /// `bytes` (read up to one byte past `cap`) cut to `cap`.
    fn of(mut bytes: Vec<u8>, cap: u64) -> Self {
        let limit = usize::try_from(cap).unwrap_or(usize::MAX);
        let truncated = bytes.len() > limit;
        bytes.truncate(limit);
        Self { bytes, truncated }
    }
}

/// Why a tool invocation did not succeed.
#[derive(Debug)]
pub(crate) enum ToolFailure {
    /// A problem the model can recover from; surfaced as an error tool result.
    Recoverable(String),
    /// Malformed arguments or an internal failure; surfaced as `Err(ToolError)`.
    Fatal(ToolError),
}

impl ToolFailure {
    pub(crate) fn recoverable(message: impl Into<String>) -> Self {
        Self::Recoverable(message.into())
    }

    /// Describe an I/O error on `display` in model-facing terms.
    pub(crate) fn io(action: &str, display: &str, err: &io::Error) -> Self {
        let message = match err.kind() {
            io::ErrorKind::NotFound => format!("file not found: {display}"),
            io::ErrorKind::PermissionDenied => {
                format!("permission denied while trying to {action} {display}")
            }
            _ => format!("failed to {action} {display}: {err}"),
        };
        Self::Recoverable(message)
    }
}

pub(crate) type ToolOutcome = Result<String, ToolFailure>;

/// One tool call: its id, the session it belongs to, the flag set when the
/// call goes, and how far a search may go.
#[derive(Debug, Clone)]
pub(crate) struct CallMeta {
    pub(crate) id: String,
    /// The calling session, from the dispatch context
    /// ([`ToolDispatchContext::origin_session_id`]), else the dispatcher's
    /// owning session, else `None` (a context-free call).
    pub(crate) session: Option<SessionId>,
    /// Set when the call's future is dropped ([`scan::run_cancellable`]).
    pub(crate) cancel: scan::Cancel,
    /// The budget of a `glob` or `grep`.
    pub(crate) limits: SearchLimits,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) probe: Option<Arc<WalkProbe>>,
}

impl CallMeta {
    /// The scan a search of `root` runs under.
    pub(crate) fn scan<'a>(&'a self, root: &'a Path) -> scan::Scan<'a> {
        let scan = scan::Scan::new(root, self.limits, &self.cancel);
        #[cfg(any(test, feature = "test-support"))]
        let scan = scan.with_probe(self.probe.clone());
        scan
    }
}

/// Key of the read-before-write record: the session that read, and the path.
type ReadKey = (Option<SessionId>, PathBuf);

/// State shared between the dispatcher and its blocking workers.
pub(crate) struct Shared {
    pub(crate) root: ProjectRoot,
    /// (session, absolute path) → mtime observed when that session last
    /// read or wrote the file. Keyed by session so one session's (or
    /// helper's) read never satisfies another session's write check. At
    /// most [`MAX_READ_RECORDS`], the oldest record dropped first (a write
    /// after that asks for the file to be read again).
    pub(crate) read_state: Mutex<indexmap::IndexMap<ReadKey, SystemTime>>,
    pub(crate) diff_tx: Option<mpsc::UnboundedSender<FileDiff>>,
    /// Skip environment files ([`crate::paths::is_secret_file_name`]) when
    /// walking for `glob`/`grep` (the classifier's read-only tools).
    pub(crate) hide_secrets: bool,
}

impl Shared {
    /// Resolve a model-supplied path against the project root.
    /// A secret path ([`ResolvedPath::secret`]: the daemon's endpoint
    /// directories) is refused here too, whatever the gate decided.
    pub(crate) fn resolve(&self, tool: &str, raw: &str) -> Result<ResolvedPath, ToolFailure> {
        let resolved = self.resolve_any(tool, raw)?;
        if resolved.secret {
            return Err(ToolFailure::recoverable(format!(
                "`{raw}` is sealed: it holds the daemon's attach token, which no file tool reads"
            )));
        }
        Ok(resolved)
    }

    fn resolve_any(&self, tool: &str, raw: &str) -> Result<ResolvedPath, ToolFailure> {
        self.root.resolve(raw).map_err(|err| match err {
            PathError::Empty | PathError::NulByte => {
                ToolFailure::Fatal(ToolError::InvalidArguments {
                    name: tool.to_string(),
                    reason: format!("invalid path: {err}"),
                })
            }
            PathError::NoHome | PathError::Io { .. } => ToolFailure::recoverable(err.to_string()),
        })
    }

    /// Display a resolved path relative to the root when possible.
    pub(crate) fn display(&self, resolved: &ResolvedPath) -> String {
        self.root.display_of(resolved).to_string()
    }

    /// Display an absolute path relative to the root when it lies inside.
    pub(crate) fn display_abs(&self, abs: &Path) -> String {
        abs.strip_prefix(self.root.as_path())
            .unwrap_or(abs)
            .display()
            .to_string()
    }

    pub(crate) fn record_read(&self, session: Option<SessionId>, abs: PathBuf, mtime: SystemTime) {
        keep_newest(
            &mut self.read_state.lock(),
            (session, abs),
            mtime,
            MAX_READ_RECORDS,
        );
    }

    /// Forward a diff to the UI. Delivery is best-effort: a closed receiver
    /// only means nobody is rendering diffs, which must not fail the tool.
    pub(crate) fn emit_diff(&self, diff: FileDiff) {
        if let Some(tx) = &self.diff_tx
            && tx.send(diff).is_err()
        {
            tracing::debug!("file diff receiver closed; dropping diff");
        }
    }

    /// Verify that an existing file may be overwritten: the final component is
    /// not a symlink, it is a regular file with no other hard link (on Unix),
    /// and it was read (and not changed
    /// since) in this session.
    ///
    /// Returns `Ok(None)` when the target does not exist yet, and the current
    /// content when it does.
    pub(crate) fn check_writable(
        &self,
        session: Option<&SessionId>,
        raw: &str,
        resolved: &ResolvedPath,
    ) -> Result<Option<String>, ToolFailure> {
        let display = self.display(resolved);
        let literal = self.root.join_literal(raw).ok();
        for candidate in [literal.as_deref(), Some(resolved.abs.as_path())]
            .into_iter()
            .flatten()
        {
            if let Ok(meta) = std::fs::symlink_metadata(candidate)
                && meta.file_type().is_symlink()
            {
                return Err(ToolFailure::recoverable(format!(
                    "refusing to write through symlink {display}; edit the link target directly"
                )));
            }
        }

        let meta = match std::fs::metadata(&resolved.abs) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(ToolFailure::io("inspect", &display, &err)),
        };
        if !meta.is_file() {
            return Err(ReadRefusal::NotRegular(meta.file_type()).into_failure("write", &display));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if meta.nlink() > 1 {
                return Err(ToolFailure::recoverable(format!(
                    "refusing to write {display}: it is a hard link shared with {} other path(s), \
                     which a write would change as well; edit the file at its own path",
                    meta.nlink() - 1
                )));
            }
        }
        let current = meta
            .modified()
            .map_err(|err| ToolFailure::io("read the modification time of", &display, &err))?;
        let recorded = self
            .read_state
            .lock()
            .get(&(session.cloned(), resolved.abs.clone()))
            .copied();
        match recorded {
            None => {
                return Err(ToolFailure::recoverable(format!(
                    "{display} has not been read since this session started (a restart forgets earlier reads); read it with read_file before modifying it"
                )));
            }
            Some(seen) if seen != current => {
                return Err(ToolFailure::recoverable(format!(
                    "{display} has been modified since it was last read (by the user or another process); read it again with read_file before modifying it"
                )));
            }
            Some(_) => {}
        }

        let read = read_capped(&resolved.abs, MAX_FILE_READ_BYTES)
            .map_err(|refusal| refusal.into_failure("read", &display))?;
        if read.truncated {
            return Err(ToolFailure::recoverable(format!(
                "{display} is larger than {} MiB; the file tools cannot rewrite it",
                MAX_FILE_READ_BYTES / (1024 * 1024)
            )));
        }
        String::from_utf8(read.bytes)
            .map(Some)
            .map_err(|_| ToolFailure::recoverable(format!("{display} is not valid UTF-8 text")))
    }

    /// Write `content` to an already-checked path, record its new mtime, and
    /// emit a diff against `old`.
    pub(crate) fn commit_write(
        &self,
        call: CallMeta,
        resolved: &ResolvedPath,
        old: &str,
        content: &str,
    ) -> Result<FileDiff, ToolFailure> {
        let display = self.display(resolved);
        // The call is gone (timed out, interrupted): nobody would see what
        // this write did.
        if call.cancel.is_cancelled() {
            return Err(ToolFailure::recoverable(format!(
                "{display} was not written: the call was cancelled"
            )));
        }
        // A temporary file renamed over the target: a hard link created
        // after `check_writable` keeps the old content, never this write.
        crate::paths::write_atomic(
            &resolved.abs,
            content.as_bytes(),
            crate::paths::FileMode::Preserve,
            None,
        )
        .map_err(|err| match err {
            crate::paths::AtomicWriteError::Io { op, source, .. } => {
                ToolFailure::io(op, &display, &source)
            }
            other => ToolFailure::recoverable(other.to_string()),
        })?;
        let mtime = std::fs::metadata(&resolved.abs)
            .and_then(|meta| meta.modified())
            .map_err(|err| ToolFailure::io("read the modification time of", &display, &err))?;
        self.record_read(call.session, resolved.abs.clone(), mtime);
        let diff = FileDiff::new(call.id, PathBuf::from(&display), old, content);
        self.emit_diff(diff.clone());
        Ok(diff)
    }
}

/// Whether `bytes` looks binary (a NUL byte within the first 8 KiB).
pub(crate) fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

/// Truncate `line` to at most `max_chars` characters, appending a marker.
pub(crate) fn truncate_line(line: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    match line.char_indices().nth(max_chars) {
        None => std::borrow::Cow::Borrowed(line),
        Some((cut, _)) => {
            let total = line.chars().count();
            std::borrow::Cow::Owned(format!("{}… [line truncated: {total} chars]", &line[..cut]))
        }
    }
}

/// Dispatcher for the in-crate file tools.
pub struct FileToolDispatcher {
    shared: Arc<Shared>,
    tools: Arc<[Arc<ToolDef>]>,
    /// The session this dispatcher serves, when it serves exactly one: the
    /// read-state key of calls whose dispatch context names no session.
    owner: Option<SessionId>,
    /// How far one `glob` or `grep` may go.
    limits: SearchLimits,
    #[cfg(any(test, feature = "test-support"))]
    probe: Option<Arc<WalkProbe>>,
}

impl FileToolDispatcher {
    /// Create a dispatcher rooted at `root`. Diffs produced by `write_file` and
    /// `edit_file` are sent to `diff_tx` when provided.
    ///
    /// The read-before-write record is kept per calling session: a call
    /// dispatched with a [`ToolDispatchContext`] naming its session
    /// ([`ToolDispatchContext::origin_session_id`], which the agent loop
    /// binds for every turn) is checked against that session's reads only.
    /// Calls without one share a single anonymous record; use
    /// [`Self::for_session`] when the dispatcher serves one known session.
    pub fn new(root: ProjectRoot, diff_tx: Option<mpsc::UnboundedSender<FileDiff>>) -> Self {
        Self::build(root, diff_tx, false)
    }

    /// A dispatcher owned by `session`: calls whose dispatch context names no
    /// session are recorded and checked as `session`'s. A context that names
    /// a session still wins.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_session(
        root: ProjectRoot,
        diff_tx: Option<mpsc::UnboundedSender<FileDiff>>,
        session: SessionId,
    ) -> Self {
        let mut dispatcher = Self::build(root, diff_tx, false);
        dispatcher.owner = Some(session);
        dispatcher
    }

    /// A dispatcher whose `glob`/`grep` never list or search environment
    /// files (for the classifier; `read_file` refusals are the caller's job).
    pub(crate) fn hiding_secrets(root: ProjectRoot) -> Self {
        Self::build(root, None, true)
    }

    fn build(
        root: ProjectRoot,
        diff_tx: Option<mpsc::UnboundedSender<FileDiff>>,
        hide_secrets: bool,
    ) -> Self {
        let tools: Arc<[Arc<ToolDef>]> = [
            read::definition(),
            write::definition(),
            edit::definition(),
            glob::definition(),
            grep::definition(),
        ]
        .into_iter()
        .map(|def| Arc::new(def.with_provenance(provenance(FILE_TOOLS_SOURCE))))
        .collect();
        Self {
            shared: Arc::new(Shared {
                root,
                read_state: Mutex::new(indexmap::IndexMap::new()),
                diff_tx,
                hide_secrets,
            }),
            tools,
            owner: None,
            limits: SearchLimits::default(),
            #[cfg(any(test, feature = "test-support"))]
            probe: None,
        }
    }

    /// The same dispatcher with `glob` and `grep` bounded by `limits`.
    #[must_use]
    pub fn with_search_limits(mut self, limits: SearchLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The same dispatcher with its searches counted (and paced) by `probe`.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_walk_probe(mut self, probe: Arc<WalkProbe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// The project root the tools resolve paths against.
    pub fn root(&self) -> &ProjectRoot {
        &self.shared.root
    }

    async fn run_blocking<A, F>(
        &self,
        call: ToolCallView<'_>,
        session: Option<SessionId>,
        run: F,
    ) -> Result<ToolResult, ToolError>
    where
        A: DeserializeOwned + Send + 'static,
        F: FnOnce(&Shared, CallMeta, A) -> ToolOutcome + Send + 'static,
    {
        let args: A = call
            .parse_args()
            .map_err(|err| ToolError::InvalidArguments {
                name: call.name.to_string(),
                reason: err.to_string(),
            })?;
        let shared = Arc::clone(&self.shared);
        let id = call.id.to_string();
        let limits = self.limits;
        #[cfg(any(test, feature = "test-support"))]
        let probe = self.probe.clone();
        let outcome = run_cancellable(move |cancel| {
            // Still queued when the call went: nothing to do for anyone.
            if cancel.is_cancelled() {
                return Err(ToolFailure::recoverable("the call was cancelled"));
            }
            let meta = CallMeta {
                id,
                session,
                cancel: cancel.clone(),
                limits,
                #[cfg(any(test, feature = "test-support"))]
                probe,
            };
            run(&shared, meta, args)
        })
        .await
        .map_err(|err| ToolError::ExecutionFailed {
            message: format!("{} worker failed: {err}", call.name),
        })?;
        let id = call.id.to_string();
        match outcome {
            Ok(text) => Ok(ToolResult::new(id, text, false)),
            Err(ToolFailure::Recoverable(message)) => Ok(ToolResult::new(id, message, true)),
            Err(ToolFailure::Fatal(err)) => Err(err),
        }
    }
}

impl std::fmt::Debug for FileToolDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileToolDispatcher")
            .field("root", &self.shared.root)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl AgentToolDispatcher for FileToolDispatcher {
    fn tools(&self) -> Arc<[Arc<ToolDef>]> {
        Arc::clone(&self.tools)
    }

    fn tool_catalog_capabilities(&self) -> meerkat_core::ToolCatalogCapabilities {
        crate::tools::fixed_catalog()
    }

    fn tool_mutation_class(&self, tool_name: &str) -> ToolMutationClass {
        match tool_name {
            READ_FILE | GLOB | GREP => ToolMutationClass::ReadOnly,
            WRITE_FILE | EDIT_FILE => ToolMutationClass::Mutating,
            _ => ToolMutationClass::Unknown,
        }
    }

    async fn dispatch(&self, call: ToolCallView<'_>) -> Result<ToolDispatchOutcome, ToolError> {
        self.dispatch_as(call, self.owner.clone()).await
    }

    async fn dispatch_with_context(
        &self,
        call: ToolCallView<'_>,
        context: &ToolDispatchContext,
    ) -> Result<ToolDispatchOutcome, ToolError> {
        let session = context
            .origin_session_id()
            .cloned()
            .or_else(|| self.owner.clone());
        self.dispatch_as(call, session).await
    }
}

impl FileToolDispatcher {
    /// Run `call` with `session` as the read-state key.
    async fn dispatch_as(
        &self,
        call: ToolCallView<'_>,
        session: Option<SessionId>,
    ) -> Result<ToolDispatchOutcome, ToolError> {
        let result = match call.name {
            READ_FILE => self.run_blocking(call, session, read::run).await,
            WRITE_FILE => self.run_blocking(call, session, write::run).await,
            EDIT_FILE => self.run_blocking(call, session, edit::run).await,
            GLOB => self.run_blocking(call, session, glob::run).await,
            GREP => self.run_blocking(call, session, grep::run).await,
            other => Err(ToolError::NotFound {
                name: other.to_string(),
            }),
        };
        result.map(ToolDispatchOutcome::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bounded record keeps the newest `cap` entries; touching an entry
    /// makes it the newest.
    #[test]
    fn keep_newest_drops_the_oldest_past_the_cap() {
        let mut map = indexmap::IndexMap::new();
        for n in 0..5 {
            keep_newest(&mut map, n, n * 10, 3);
        }
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), [2, 3, 4]);
        keep_newest(&mut map, 2, 21, 3);
        keep_newest(&mut map, 5, 50, 3);
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), [4, 2, 5]);
        assert_eq!(map.get(&2), Some(&21));
    }

    #[test]
    fn truncate_line_keeps_short_lines() {
        assert_eq!(truncate_line("abc", 5), "abc");
        assert_eq!(truncate_line("abcde", 5), "abcde");
    }

    #[test]
    fn truncate_line_marks_long_lines() {
        let out = truncate_line("abcdefg", 3);
        assert!(out.starts_with("abc… [line truncated: 7 chars]"));
    }

    #[test]
    fn truncate_line_respects_char_boundaries() {
        let out = truncate_line("ééééé", 2);
        assert!(out.starts_with("éé…"));
    }

    #[test]
    fn binary_detection_only_sniffs_prefix() {
        assert!(looks_binary(b"ab\0cd"));
        assert!(!looks_binary(b"plain text"));
        let mut late = vec![b'a'; BINARY_SNIFF_BYTES];
        late.push(0);
        assert!(!looks_binary(&late));
    }
}
