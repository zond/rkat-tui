//! What a conversation's context holds, as the daemon sees it
//! (`rkat/context`, `/context`): its messages by kind and the system
//! prompt's size, read from the session's history, and the tool
//! definitions its model requests carry, by family, from the catalog the
//! daemon reads when the agent is idle ([`super::approvals::DaemonToolOrigins`]).

use meerkat_core::{Message, ToolDef, ToolSourceKind};
use serde::{Deserialize, Serialize};

/// Where a tool definition comes from, as `/context` groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFamily {
    /// The engine's built-in tools and its shell.
    Builtin,
    /// The engine's comms tools (peer messages).
    Comms,
    /// rkat-tui's file tools.
    Files,
    /// The engine's semantic memory.
    Memory,
    /// The engine's mob tools (`delegate`, `mob_*`, councils).
    Mob,
    /// rkat-tui's note tools.
    Notes,
    /// The engine's schedule tools.
    Schedule,
    /// rkat-tui's watch tools.
    Watch,
    /// Another host callback tool.
    Callback,
    /// The engine's work graph tools.
    Workgraph,
    /// An MCP server's tools.
    Mcp,
    /// A Rust tool bundle's.
    Bundle,
    /// The deferred-catalog control tools: the engine's, and rkat-tui's
    /// own search (`super::deferral::reveal`).
    Catalog,
    /// No provenance.
    Other,
}

impl ToolFamily {
    /// Every family.
    #[cfg(test)]
    const ALL: [Self; 14] = [
        Self::Builtin,
        Self::Comms,
        Self::Files,
        Self::Memory,
        Self::Mob,
        Self::Notes,
        Self::Schedule,
        Self::Watch,
        Self::Callback,
        Self::Workgraph,
        Self::Mcp,
        Self::Bundle,
        Self::Catalog,
        Self::Other,
    ];

    /// Its name as `/context` prints it: the wire name (a test pins them
    /// equal).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Comms => "comms",
            Self::Files => "files",
            Self::Memory => "memory",
            Self::Mob => "mob",
            Self::Notes => "notes",
            Self::Schedule => "schedule",
            Self::Watch => "watch",
            Self::Callback => "callback",
            Self::Workgraph => "workgraph",
            Self::Mcp => "mcp",
            Self::Bundle => "bundle",
            Self::Catalog => "catalog",
            Self::Other => "other",
        }
    }

    /// The family of `def`.
    #[must_use]
    pub fn of(def: &ToolDef) -> Self {
        if matches!(
            def.name.as_str(),
            meerkat_core::tool_catalog::TOOL_CATALOG_SEARCH_NAME
                | meerkat_core::tool_catalog::TOOL_CATALOG_LOAD_NAME
                | super::deferral::reveal::TOOL_SEARCH
        ) {
            return Self::Catalog;
        }
        let Some(provenance) = &def.provenance else {
            return Self::Other;
        };
        match provenance.kind {
            ToolSourceKind::Callback => match provenance.source_id.as_str() {
                crate::tools::FILE_TOOLS_SOURCE => Self::Files,
                crate::memory::NOTES_SOURCE => Self::Notes,
                crate::tools::WATCH_TOOLS_SOURCE => Self::Watch,
                _ => Self::Callback,
            },
            ToolSourceKind::Builtin | ToolSourceKind::Shell => Self::Builtin,
            ToolSourceKind::Memory => Self::Memory,
            ToolSourceKind::Comms => Self::Comms,
            ToolSourceKind::Schedule => Self::Schedule,
            ToolSourceKind::WorkGraph => Self::Workgraph,
            ToolSourceKind::Mob => Self::Mob,
            ToolSourceKind::Mcp => Self::Mcp,
            ToolSourceKind::RustBundle => Self::Bundle,
        }
    }
}

/// The bytes one tool definition costs a request: its name, description
/// and input schema as JSON (what a provider is sent for it).
#[must_use]
pub fn def_bytes(def: &ToolDef) -> usize {
    serde_json::json!({
        "name": def.name.as_str(),
        "description": def.description,
        "input_schema": def.input_schema,
    })
    .to_string()
    .len()
}

/// One family's share of a request's tool definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilySize {
    /// The family.
    pub family: ToolFamily,
    /// Its tools.
    pub tools: usize,
    /// Their definitions' bytes.
    pub bytes: usize,
}

/// `defs` by family, the largest first.
#[must_use]
pub fn family_sizes<'a>(defs: impl IntoIterator<Item = &'a ToolDef>) -> Vec<FamilySize> {
    let mut by: std::collections::BTreeMap<ToolFamily, FamilySize> =
        std::collections::BTreeMap::new();
    for def in defs {
        let family = ToolFamily::of(def);
        let size = by.entry(family).or_insert(FamilySize {
            family,
            tools: 0,
            bytes: 0,
        });
        size.tools += 1;
        size.bytes += def_bytes(def);
    }
    let mut sizes: Vec<FamilySize> = by.into_values().collect();
    sizes.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.family.cmp(&b.family)));
    sizes
}

/// A conversation's messages by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageCounts {
    /// System messages.
    pub system: usize,
    /// Their text's bytes (the system prompt).
    pub system_bytes: usize,
    /// User messages.
    pub user: usize,
    /// Assistant messages.
    pub assistant: usize,
    /// Tool-result messages.
    pub tool_results: usize,
    /// System notices.
    pub notices: usize,
}

impl MessageCounts {
    /// The counts of `messages`.
    #[must_use]
    pub fn of(messages: &[Message]) -> Self {
        let mut counts = Self::default();
        for message in messages {
            match message {
                Message::System(system) => {
                    counts.system += 1;
                    counts.system_bytes += system.content.len();
                }
                Message::SystemNotice(_) => counts.notices += 1,
                Message::User(_) => counts.user += 1,
                Message::BlockAssistant(_) => counts.assistant += 1,
                Message::ToolResults { .. } => counts.tool_results += 1,
            }
        }
        counts
    }

    /// Every message but the system prompt.
    #[must_use]
    pub fn total(&self) -> usize {
        self.user + self.assistant + self.tool_results + self.notices
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `/context` prints each family under its wire name: one vocabulary.
    #[test]
    fn each_family_is_printed_under_its_wire_name() {
        for family in ToolFamily::ALL {
            assert_eq!(
                serde_json::to_value(family).unwrap(),
                serde_json::Value::String(family.name().to_string())
            );
        }
    }
}
