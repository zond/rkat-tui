//! Where a session's tools come from, as the approval policy sees it.
//!
//! A tool's name says nothing about who implements it: an MCP server may
//! offer a tool called `read_file` or `shell`. The tier policy therefore
//! treats a name as a builtin, a file tool or a note tool only when the
//! session's live tool definitions say so ([`ToolProvenance`]):
//!
//! - engine-native provenance (`builtin`, `shell`, `comms`, `memory`,
//!   `schedule`, `workgraph`, `mob`) ⇒ [`ToolFamily::Native`];
//! - the TUI's own callback surfaces ([`FILE_TOOLS_SOURCE`],
//!   [`NOTES_SOURCE`]) ⇒ [`ToolFamily::Files`] / [`ToolFamily::Notes`];
//! - MCP, Rust bundles, any other callback surface, a definition without
//!   provenance and a name the session does not offer ⇒
//!   [`ToolFamily::Foreign`], which the policy always sends to the
//!   classifier unless a user rule decides it.
//!
//! The gate learns a session's definitions from a [`ToolOrigins`] source at
//! every call (the runtime's published visible tool definitions), so a tool
//! that appears or is replaced mid-session is judged by what it is now.

use std::collections::HashMap;

use async_trait::async_trait;
use meerkat_core::ToolCatalogDeferredEligibility::DeferredEligible;
use meerkat_core::{
    SessionId, ToolCatalogView, ToolCatalogVisibility, ToolDef, ToolMutationClass, ToolProvenance,
    ToolSourceKind,
};

use crate::memory::{NOTE_FORGET, NOTE_LIST, NOTE_READ, NOTE_SAVE, NOTES_SOURCE};
use crate::tools::{
    EDIT_FILE, FILE_TOOLS_SOURCE, GLOB, GREP, READ_FILE, UNWATCH, WATCH_PATHS, WATCH_TOOLS_SOURCE,
    WRITE_FILE,
};

/// Which implementation family a tool belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolFamily {
    /// An engine builtin (native provenance).
    Native,
    /// The TUI's file tools.
    Files,
    /// The TUI's note tools.
    Notes,
    /// Anything else: MCP, bundles, other callback surfaces, no provenance,
    /// or a name the session does not offer.
    Foreign,
}

/// Where one tool comes from and what it declares about mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOrigin {
    /// The definition's provenance, if it carries one.
    pub provenance: Option<ToolProvenance>,
    /// The tool's declared mutation class (`Unknown` when undeclared).
    pub mutation: ToolMutationClass,
}

impl ToolOrigin {
    /// An engine-native tool of `kind`.
    #[must_use]
    pub fn native(kind: ToolSourceKind) -> Self {
        Self {
            provenance: Some(ToolProvenance::new(kind, "builtin")),
            mutation: ToolMutationClass::Unknown,
        }
    }

    /// A tool of the TUI's own callback surface `source`
    /// ([`FILE_TOOLS_SOURCE`] or [`NOTES_SOURCE`]), with the mutation class
    /// the TUI declares for `name`.
    #[must_use]
    pub fn tui(source: &str, name: &str) -> Self {
        Self {
            provenance: Some(crate::tools::provenance(source)),
            mutation: tui_mutation(name),
        }
    }

    /// A tool of an MCP server.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn mcp(server: &str) -> Self {
        Self {
            provenance: Some(ToolProvenance::new(ToolSourceKind::Mcp, server)),
            mutation: ToolMutationClass::Unknown,
        }
    }

    /// The origin of the tool `name` whose definition declares
    /// `provenance`: the TUI's own tools get the mutation class the TUI
    /// declares for them, every other tool `Unknown`.
    fn declared(name: &str, provenance: Option<ToolProvenance>) -> Self {
        let mut origin = Self {
            provenance,
            mutation: ToolMutationClass::Unknown,
        };
        if matches!(origin.family(), ToolFamily::Files | ToolFamily::Notes) {
            origin.mutation = tui_mutation(name);
        }
        origin
    }

    /// The family this origin puts its tool in.
    #[must_use]
    pub fn family(&self) -> ToolFamily {
        let Some(provenance) = &self.provenance else {
            return ToolFamily::Foreign;
        };
        match provenance.kind {
            ToolSourceKind::Builtin
            | ToolSourceKind::Shell
            | ToolSourceKind::Comms
            | ToolSourceKind::Memory
            | ToolSourceKind::Schedule
            | ToolSourceKind::WorkGraph
            | ToolSourceKind::Mob => ToolFamily::Native,
            ToolSourceKind::Callback
                if provenance.source_id.as_str() == FILE_TOOLS_SOURCE
                    || provenance.source_id.as_str() == WATCH_TOOLS_SOURCE =>
            {
                ToolFamily::Files
            }
            ToolSourceKind::Callback if provenance.source_id.as_str() == NOTES_SOURCE => {
                ToolFamily::Notes
            }
            ToolSourceKind::Callback | ToolSourceKind::Mcp | ToolSourceKind::RustBundle => {
                ToolFamily::Foreign
            }
        }
    }
}

/// The mutation class the TUI declares for its own tools.
fn tui_mutation(name: &str) -> ToolMutationClass {
    match name {
        READ_FILE | GLOB | GREP | NOTE_LIST | NOTE_READ | WATCH_PATHS | UNWATCH => {
            ToolMutationClass::ReadOnly
        }
        WRITE_FILE | EDIT_FILE | NOTE_SAVE | NOTE_FORGET => ToolMutationClass::Mutating,
        _ => ToolMutationClass::Unknown,
    }
}

/// A session's tools by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCatalog {
    tools: HashMap<String, ToolOrigin>,
}

impl ToolCatalog {
    /// The catalog of `defs`. The TUI's own tools get the mutation class
    /// the TUI declares for them; every other tool `Unknown`.
    pub fn from_defs<'a>(defs: impl IntoIterator<Item = &'a ToolDef>) -> Self {
        let tools = defs
            .into_iter()
            .map(|def| {
                let name = def.name.to_string();
                let origin = ToolOrigin::declared(&name, def.provenance.clone());
                (name, origin)
            })
            .collect();
        Self { tools }
    }

    /// Add every tool `view` keeps deferred (behind the deferred catalog's
    /// `tool_catalog_search` and `tool_catalog_load`) that this catalog does
    /// not name yet, with the owner its entry declares: the visible
    /// definitions leave such a tool out until the session loads it, and
    /// its first call, in the run that loads it, is judged by what it is
    /// rather than as a foreign tool.
    pub fn add_deferred(&mut self, view: &ToolCatalogView) {
        for entry in &view.entries {
            let (ToolCatalogVisibility::Deferred, DeferredEligible { provenance }) =
                (entry.visibility, &entry.deferred_eligibility)
            else {
                continue;
            };
            let name = entry.name.to_string();
            if self.tools.contains_key(&name) {
                continue;
            }
            let origin = ToolOrigin::declared(&name, Some(provenance.clone()));
            self.tools.insert(name, origin);
        }
    }

    /// Add or replace one tool.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn with(mut self, name: impl Into<String>, origin: ToolOrigin) -> Self {
        self.tools.insert(name.into(), origin);
        self
    }

    /// The origin of `name`, if the session offers it.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ToolOrigin> {
        self.tools.get(name)
    }

    /// The engine-native tools of this catalog
    /// ([`ToolFamily::Native`]).
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn natives(&self) -> Self {
        Self {
            tools: self
                .tools
                .iter()
                .filter(|(_, origin)| origin.family() == ToolFamily::Native)
                .map(|(name, origin)| (name.clone(), origin.clone()))
                .collect(),
        }
    }

    /// Add, in place, every engine-native tool of `other` this catalog
    /// does not name yet (no copy of either catalog).
    pub fn add_missing_natives(&mut self, other: &Self) {
        for (name, origin) in &other.tools {
            if origin.family() == ToolFamily::Native && !self.tools.contains_key(name) {
                self.tools.insert(name.clone(), origin.clone());
            }
        }
    }

    /// Number of tools.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the catalog is empty.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Source of a session's live tool definitions.
#[async_trait]
pub trait ToolOrigins: Send + Sync {
    /// The tools `session_id` currently offers. `None` when they cannot be
    /// read: every call is then judged as a [`ToolFamily::Foreign`] tool.
    async fn catalog(&self, session_id: &SessionId) -> Option<ToolCatalog>;
}

/// [`ToolOrigins`] that always answers with one fixed catalog (tests and
/// embedders whose tool set is static).
#[derive(Debug, Clone, Default)]
#[cfg(any(test, feature = "test-support"))]
pub struct StaticToolOrigins(pub ToolCatalog);

#[async_trait]
#[cfg(any(test, feature = "test-support"))]
impl ToolOrigins for StaticToolOrigins {
    async fn catalog(&self, _session_id: &SessionId) -> Option<ToolCatalog> {
        Some(self.0.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A tool the session keeps deferred is named with the owner its catalog
    /// entry declares; one the visible definitions name, a hidden one and
    /// one that may not be deferred are not touched.
    #[test]
    fn deferred_tools_are_named_by_their_declared_owner() {
        use meerkat_core::{
            ToolCallability, ToolCatalogMode, ToolCatalogViewEntry, ToolPlaneClass,
        };
        let schedule = ToolProvenance::new(ToolSourceKind::Schedule, "schedule");
        let entry =
            |name: &str, visibility, eligible: Option<&ToolProvenance>| ToolCatalogViewEntry {
                name: name.into(),
                plane: ToolPlaneClass::Session,
                deferred_eligibility: eligible.map_or(
                    meerkat_core::ToolCatalogDeferredEligibility::InlineOnly,
                    |provenance| DeferredEligible {
                        provenance: provenance.clone(),
                    },
                ),
                callability: ToolCallability::Callable,
                visibility,
            };
        let view = ToolCatalogView {
            mode: ToolCatalogMode::Deferred,
            exact_catalog: true,
            pending_sources: Vec::new(),
            staged_changes_pending: false,
            entries: vec![
                entry(
                    "meerkat_schedule_get",
                    ToolCatalogVisibility::Deferred,
                    Some(&schedule),
                ),
                entry(
                    NOTE_SAVE,
                    ToolCatalogVisibility::Deferred,
                    Some(&crate::tools::provenance(NOTES_SOURCE)),
                ),
                // Visible already: the definition's owner stands.
                entry(
                    "shell",
                    ToolCatalogVisibility::Deferred,
                    Some(&ToolProvenance::new(ToolSourceKind::Mcp, "evil")),
                ),
                entry("hidden", ToolCatalogVisibility::Hidden, Some(&schedule)),
                entry("inline", ToolCatalogVisibility::Visible, None),
            ],
        };
        let defs = [ToolDef::new("shell", "", json!({}))
            .with_provenance(ToolProvenance::new(ToolSourceKind::Shell, "shell"))];
        let mut catalog = ToolCatalog::from_defs(&defs);
        catalog.add_deferred(&view);
        assert_eq!(catalog.len(), 3);
        let family = |n: &str| catalog.get(n).map(ToolOrigin::family);
        assert_eq!(family("meerkat_schedule_get"), Some(ToolFamily::Native));
        assert_eq!(family(NOTE_SAVE), Some(ToolFamily::Notes));
        assert_eq!(
            catalog.get(NOTE_SAVE).map(|o| o.mutation),
            Some(ToolMutationClass::Mutating)
        );
        assert_eq!(family("shell"), Some(ToolFamily::Native));
        assert_eq!(family("hidden"), None);
        assert_eq!(family("inline"), None);
    }

    #[test]
    fn families_follow_provenance_not_names() {
        let defs = [
            ToolDef::new("shell", "", json!({}))
                .with_provenance(ToolProvenance::new(ToolSourceKind::Builtin, "builtin")),
            ToolDef::new(READ_FILE, "", json!({}))
                .with_provenance(crate::tools::provenance(FILE_TOOLS_SOURCE)),
            ToolDef::new(NOTE_SAVE, "", json!({}))
                .with_provenance(crate::tools::provenance(NOTES_SOURCE)),
            // An MCP server offering a tool named like a builtin.
            ToolDef::new("task_list", "", json!({}))
                .with_provenance(ToolProvenance::new(ToolSourceKind::Mcp, "evil")),
            // A foreign callback surface claiming a TUI tool name.
            ToolDef::new(WRITE_FILE, "", json!({}))
                .with_provenance(crate::tools::provenance("someone-else")),
            ToolDef::new("glob", "", json!({})),
        ];
        let catalog = ToolCatalog::from_defs(&defs);
        let family = |n: &str| catalog.get(n).map(ToolOrigin::family);
        assert_eq!(family("shell"), Some(ToolFamily::Native));
        assert_eq!(family(READ_FILE), Some(ToolFamily::Files));
        assert_eq!(family(NOTE_SAVE), Some(ToolFamily::Notes));
        assert_eq!(family("task_list"), Some(ToolFamily::Foreign));
        assert_eq!(family(WRITE_FILE), Some(ToolFamily::Foreign));
        assert_eq!(family("glob"), Some(ToolFamily::Foreign));
        assert_eq!(family("absent"), None);
        assert_eq!(
            catalog.get(READ_FILE).map(|o| o.mutation),
            Some(ToolMutationClass::ReadOnly)
        );
        // The natives alone, and a catalog extended with them: what the
        // daemon judges a session it has not read yet by.
        let natives = catalog.natives();
        assert_eq!(natives.len(), 1);
        assert_eq!(
            natives.get("shell").map(ToolOrigin::family),
            Some(ToolFamily::Native)
        );
        let mut joined =
            ToolCatalog::default().with(WRITE_FILE, ToolOrigin::tui(FILE_TOOLS_SOURCE, WRITE_FILE));
        joined.add_missing_natives(&catalog);
        // Only the natives come in; what the catalog already names stays.
        assert_eq!(joined.len(), 2);
        assert_eq!(
            joined.get(WRITE_FILE).map(ToolOrigin::family),
            Some(ToolFamily::Files)
        );
        assert_eq!(
            joined.get("shell").map(ToolOrigin::family),
            Some(ToolFamily::Native)
        );
        let mut shadowed =
            ToolCatalog::default().with("shell", ToolOrigin::tui(FILE_TOOLS_SOURCE, "shell"));
        shadowed.add_missing_natives(&catalog);
        assert_eq!(
            shadowed.get("shell").map(ToolOrigin::family),
            Some(ToolFamily::Files)
        );
        assert_eq!(
            catalog.get(NOTE_SAVE).map(|o| o.mutation),
            Some(ToolMutationClass::Mutating)
        );
    }
}
