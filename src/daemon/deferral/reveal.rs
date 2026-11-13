//! rkat-tui's own search and reveal: the search tool and each
//! conversation's revealed tools.
//!
//! **Removed when the engine's native deferral ships**
//! (lukacf/meerkat#2180: an exact catalog for a mob member's dispatchers,
//! so its `tool_catalog_search` and `tool_catalog_load` engage). Then the
//! parent module relabels the deferrable tools as deferred-eligible
//! instead of hiding them, and this module, its file and the gate's line
//! for [`TOOL_SEARCH`] go.
//!
//! A search (free text and/or a family) answers the matching hidden tools
//! (name and one-line description) and reveals exactly those for the
//! conversation: from its next model request on they are declared with
//! their full schemas. The reveals are kept per conversation (session) in
//! `<realm>/tui_tools.json`, so a daemon restart, a reseat, a compaction
//! and a resumed conversation keep them, and a new conversation starts
//! with none. A conversation stays resumable as long as the realm, so its
//! reveals are kept as long (a line per conversation that searched).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use meerkat_core::ops::ToolDispatchOutcome;
use meerkat_core::{
    AgentToolDispatcher, SessionId, ToolCallView, ToolDef, ToolError, ToolMutationClass, ToolResult,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Deferrable, HiddenFamily, Shown};
use crate::paths::{FileMode, write_atomic};

/// Tool name: search the hidden tools and reveal the matches.
pub const TOOL_SEARCH: &str = "tool_search";
/// Provenance source id of [`TOOL_SEARCH`] (judged with rkat-tui's own
/// read-only tools, see `crate::approval::catalog`).
pub const TOOL_SEARCH_SOURCE: &str = "rkat-tui.tool-search";
/// The reveals' file in the realm.
pub const REVEALS_FILE: &str = "tui_tools.json";

/// The file's content.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    /// By conversation, the tools it revealed.
    revealed: BTreeMap<SessionId, BTreeSet<String>>,
}

/// Every conversation's revealed tools, kept in the realm's
/// [`REVEALS_FILE`].
pub struct Reveals {
    path: PathBuf,
    revealed: RwLock<BTreeMap<SessionId, BTreeSet<String>>>,
    loaded: tokio::sync::OnceCell<()>,
    /// Serializes the writes, so the last change is the one on disk.
    writing: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Reveals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reveals")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Reveals {
    /// The reveals of the realm at `realm_root` (read on first use).
    #[must_use]
    pub fn new(realm_root: &std::path::Path) -> Self {
        Self {
            path: realm_root.join(REVEALS_FILE),
            revealed: RwLock::new(BTreeMap::new()),
            loaded: tokio::sync::OnceCell::new(),
            writing: tokio::sync::Mutex::new(()),
        }
    }

    /// Read the file, once (a missing or unreadable file is no reveals;
    /// an unreadable one is logged and replaced at the next reveal).
    pub async fn load(&self) {
        self.loaded
            .get_or_init(|| async {
                let path = self.path.clone();
                let read = tokio::task::spawn_blocking(move || std::fs::read_to_string(&path)).await;
                let stored = match read {
                    Ok(Ok(text)) => match serde_json::from_str::<Stored>(&text) {
                        Ok(stored) => stored,
                        Err(err) => {
                            tracing::warn!(error = %err, path = %self.path.display(), "the revealed tools could not be read; starting with none");
                            Stored::default()
                        }
                    },
                    Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                        Stored::default()
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(error = %err, path = %self.path.display(), "the revealed tools could not be read; starting with none");
                        Stored::default()
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "the revealed tools' read did not finish");
                        Stored::default()
                    }
                };
                let mut revealed = self.revealed.write();
                for (session, names) in stored.revealed {
                    revealed.entry(session).or_default().extend(names);
                }
            })
            .await;
    }

    /// The tools `session` revealed.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn revealed(&self, session: &SessionId) -> BTreeSet<String> {
        self.revealed
            .read()
            .get(session)
            .cloned()
            .unwrap_or_default()
    }

    /// Reveal `names` for `session`, written before this returns.
    ///
    /// # Errors
    ///
    /// The file could not be written (the reveal holds until the daemon
    /// stops).
    pub async fn reveal(&self, session: &SessionId, names: &[String]) -> Result<(), String> {
        self.load().await;
        {
            let mut revealed = self.revealed.write();
            let set = revealed.entry(session.clone()).or_default();
            if names.iter().all(|name| set.contains(name)) {
                return Ok(());
            }
            set.extend(names.iter().cloned());
        }
        self.persist().await
    }

    async fn persist(&self) -> Result<(), String> {
        let _writing = self.writing.lock().await;
        let stored = Stored {
            revealed: self.revealed.read().clone(),
        };
        let text = serde_json::to_string_pretty(&stored).map_err(|err| err.to_string())?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            write_atomic(&path, text.as_bytes(), FileMode::Private, None)
        })
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
    }
}

impl Shown for Reveals {
    fn shown(&self, session: &SessionId, name: &str) -> bool {
        self.revealed
            .read()
            .get(session)
            .is_some_and(|names| names.contains(name))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    family: Option<String>,
}

/// The longest one-line description a search answers with.
const LINE: usize = 160;

/// The first sentence or line of `description`, at most [`LINE`] bytes.
fn one_line(description: &str) -> String {
    let first = description.lines().next().unwrap_or_default().trim();
    let sentence = first
        .find(". ")
        .map_or(first, |end| &first[..=end])
        .trim_end();
    if sentence.len() <= LINE {
        return sentence.to_string();
    }
    let mut cut = LINE;
    while !sentence.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &sentence[..cut])
}

/// One conversation's search tool.
pub struct SearchTool {
    session: SessionId,
    reveals: Arc<Reveals>,
    deferrable: Arc<Deferrable>,
    tools: Arc<[Arc<ToolDef>]>,
}

impl std::fmt::Debug for SearchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchTool")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// The search tool's definition.
#[must_use]
pub fn definition() -> ToolDef {
    let families: Vec<&str> = HiddenFamily::ALL.iter().map(|f| f.name()).collect();
    ToolDef::new(
        TOOL_SEARCH,
        "Find tools left out of your tool list and make them available. Every word of `query` must appear in a tool's name or description (a query of exact tool names finds just those); `family` limits the search to one family (`schedule`: scheduling yourself, `mob`: teams, councils, forks, mob profiles). Returns each match's name and summary; from your next step on the matches are in your tool list with their full schemas and you call them directly. A hidden tool you call without searching first is not found.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "family": {"type": "string", "enum": families}
            },
            "additionalProperties": false
        }),
    )
    .with_provenance(crate::tools::provenance(TOOL_SEARCH_SOURCE))
}

impl SearchTool {
    /// `session`'s search over the `deferrable` tools.
    #[must_use]
    pub fn new(session: SessionId, reveals: Arc<Reveals>, deferrable: Arc<Deferrable>) -> Self {
        Self {
            session,
            reveals,
            deferrable,
            tools: vec![Arc::new(definition())].into(),
        }
    }

    /// The deferrable tools matching `args`.
    fn matches(&self, args: &SearchArgs) -> Vec<Arc<ToolDef>> {
        let words: Vec<String> = args
            .query
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let family = args.family.as_deref().and_then(HiddenFamily::parse);
        // A query naming tools exactly reveals just those.
        let named: Vec<Arc<ToolDef>> = self
            .deferrable
            .all()
            .into_iter()
            .filter(|def| words.contains(&def.name.to_lowercase()))
            .collect();
        if !named.is_empty() && named.len() == words.len() {
            return named;
        }
        self.deferrable
            .all()
            .into_iter()
            .filter(|def| family.is_none_or(|family| HiddenFamily::of(def) == Some(family)))
            .filter(|def| {
                let text = format!("{} {}", def.name, def.description).to_lowercase();
                words.iter().all(|word| text.contains(word.as_str()))
            })
            .collect()
    }

    async fn search(&self, id: String, args: SearchArgs) -> ToolResult {
        let unknown_family = args
            .family
            .as_deref()
            .filter(|name| HiddenFamily::parse(name).is_none());
        if let Some(name) = unknown_family {
            return ToolResult::new(id, format!("no family `{name}`; {}", self.families()), true);
        }
        if args.query.as_deref().is_none_or(|q| q.trim().is_empty()) && args.family.is_none() {
            return ToolResult::new(
                id,
                format!("give a `query`, a `family` or both; {}", self.families()),
                true,
            );
        }
        let found = self.matches(&args);
        if found.is_empty() {
            return ToolResult::new(
                id,
                format!(
                    "no hidden tool matches; search with fewer or broader words (one word such \
                     as `schedule` often does), or pass a `family` alone to list all its tools; {}",
                    self.families()
                ),
                false,
            );
        }
        let names: Vec<String> = found.iter().map(|def| def.name.to_string()).collect();
        if let Err(err) = self.reveals.reveal(&self.session, &names).await {
            tracing::warn!(error = %err, "the revealed tools could not be written");
        }
        let lines: Vec<String> = found
            .iter()
            .map(|def| format!("- {}: {}", def.name, one_line(&def.description)))
            .collect();
        ToolResult::new(
            id,
            format!(
                "Available from your next step on (call them directly):\n{}",
                lines.join("\n")
            ),
            false,
        )
    }

    /// The families and their sizes, for a search that found nothing.
    fn families(&self) -> String {
        let all = self.deferrable.all();
        let counts: Vec<String> = HiddenFamily::ALL
            .iter()
            .map(|family| {
                let n = all
                    .iter()
                    .filter(|def| HiddenFamily::of(def) == Some(*family))
                    .count();
                format!("`{}` ({n} tools)", family.name())
            })
            .collect();
        format!("families: {}", counts.join(", "))
    }
}

#[async_trait]
impl AgentToolDispatcher for SearchTool {
    fn tools(&self) -> Arc<[Arc<ToolDef>]> {
        Arc::clone(&self.tools)
    }

    fn tool_catalog_capabilities(&self) -> meerkat_core::ToolCatalogCapabilities {
        crate::tools::fixed_catalog()
    }

    fn tool_mutation_class(&self, tool_name: &str) -> ToolMutationClass {
        match tool_name {
            TOOL_SEARCH => ToolMutationClass::ReadOnly,
            _ => ToolMutationClass::Unknown,
        }
    }

    async fn dispatch(&self, call: ToolCallView<'_>) -> Result<ToolDispatchOutcome, ToolError> {
        if call.name != TOOL_SEARCH {
            return Err(ToolError::NotFound {
                name: call.name.to_string(),
            });
        }
        let args: SearchArgs = call
            .parse_args()
            .map_err(|err| ToolError::InvalidArguments {
                name: call.name.to_string(),
                reason: err.to_string(),
            })?;
        Ok(ToolDispatchOutcome::from(
            self.search(call.id.to_string(), args).await,
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use meerkat_core::{ToolProvenance, ToolSourceKind};

    fn deferrable() -> Arc<Deferrable> {
        let registry = Arc::new(Deferrable::new());
        let def = |name: &str, kind, description: &str| {
            Arc::new(
                ToolDef::new(name, description, json!({}))
                    .with_provenance(ToolProvenance::new(kind, "x")),
            )
        };
        registry.note(
            &[
                def(
                    "meerkat_schedule_create",
                    ToolSourceKind::Schedule,
                    "Create a schedule. More text.",
                ),
                def(
                    "meerkat_schedule_list",
                    ToolSourceKind::Schedule,
                    "List schedules.",
                ),
                def(
                    "council",
                    ToolSourceKind::Mob,
                    "Convene a council of helpers.",
                ),
                def("delegate", ToolSourceKind::Mob, "Direct: never noted."),
            ],
            &super::super::DIRECT_MOB_TOOLS,
        );
        registry
    }

    fn call(args: serde_json::Value) -> String {
        args.to_string()
    }

    async fn search(tool: &SearchTool, args: serde_json::Value) -> (String, bool) {
        let raw = serde_json::value::RawValue::from_string(call(args)).unwrap();
        let outcome = tool
            .dispatch(ToolCallView {
                id: "c1",
                name: TOOL_SEARCH,
                args: &raw,
            })
            .await
            .unwrap();
        let result = outcome.result;
        (result.text_content(), result.is_error)
    }

    /// A search reveals exactly its matches for its conversation, kept in
    /// the realm's file; another conversation reveals nothing, and a fresh
    /// reader sees the same.
    #[tokio::test]
    async fn a_search_reveals_exactly_its_matches_for_its_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let reveals = Arc::new(Reveals::new(dir.path()));
        let session = SessionId::new();
        let other = SessionId::new();
        let tool = SearchTool::new(session.clone(), Arc::clone(&reveals), deferrable());
        let (text, error) = search(&tool, json!({"query": "create SCHEDULE"})).await;
        assert!(!error, "{text}");
        assert!(
            text.contains("- meerkat_schedule_create: Create a schedule."),
            "{text}"
        );
        assert!(!text.contains("meerkat_schedule_list"), "{text}");
        assert!(reveals.shown(&session, "meerkat_schedule_create"));
        assert!(!reveals.shown(&session, "meerkat_schedule_list"));
        assert!(!reveals.shown(&other, "meerkat_schedule_create"));
        // Exact names find just those, though `schedule` is in both.
        let (text, _) = search(&tool, json!({"query": "meerkat_schedule_list"})).await;
        assert!(
            text.contains("meerkat_schedule_list") && !text.contains("create"),
            "{text}"
        );
        let (text, _) = search(&tool, json!({"family": "mob"})).await;
        assert!(
            text.contains("council") && !text.contains("delegate"),
            "{text}"
        );
        let read = Reveals::new(dir.path());
        read.load().await;
        assert_eq!(
            read.revealed(&session),
            BTreeSet::from([
                "council".to_string(),
                "meerkat_schedule_create".to_string(),
                "meerkat_schedule_list".to_string()
            ])
        );
    }

    /// A search without a match, without terms or with an unknown family
    /// says which families there are and reveals nothing.
    #[tokio::test]
    async fn a_search_without_a_match_names_the_families() {
        let dir = tempfile::tempdir().unwrap();
        let reveals = Arc::new(Reveals::new(dir.path()));
        let session = SessionId::new();
        let tool = SearchTool::new(session.clone(), Arc::clone(&reveals), deferrable());
        let (text, error) = search(&tool, json!({"query": "weather"})).await;
        assert!(!error);
        assert!(
            text.contains(
                "no hidden tool matches; search with fewer or broader words (one word such as \
                 `schedule` often does), or pass a `family` alone to list all its tools; \
                 families: `schedule` (2 tools), `mob` (1 tools)"
            ),
            "{text}"
        );
        let (text, error) = search(&tool, json!({})).await;
        assert!(error && text.starts_with("give a `query`"), "{text}");
        let (text, error) = search(&tool, json!({"family": "nope"})).await;
        assert!(error && text.starts_with("no family `nope`"), "{text}");
        assert!(reveals.revealed(&session).is_empty());
        assert!(!dir.path().join(REVEALS_FILE).exists());
    }

    #[test]
    fn a_description_is_cut_to_its_first_sentence() {
        assert_eq!(one_line("Create it. Then more."), "Create it.");
        assert_eq!(one_line("One line\nsecond"), "One line");
        let long = "x".repeat(400);
        assert_eq!(one_line(&long).chars().count(), LINE + 1);
    }
}
