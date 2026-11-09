//! Notes: small Markdown files the agent saves for later sessions (distinct
//! from the engine's native semantic memory, `memory_search`).
//!
//! Two scopes exist: user-level notes in `<config>/notes/` (durable personal
//! preferences that follow the user into every project) and workspace notes in
//! `<workspace>/.rkat/tui/notes/` (facts about this project). Each directory
//! holds one `<slug>.md` per note plus a regenerated `MEMORY.md` index.
//!
//! - [`store`] — [`NoteStore`]: slugging, caps, atomic owner-only writes,
//!   symlink refusal below the trusted root, index generation and the
//!   untrusted system-prompt section ([`prompt_index`]).
//! - [`tools`] — [`NoteToolDispatcher`]: `note_save`, `note_read`,
//!   `note_forget`, `note_list`.

pub mod store;
pub mod tools;

pub use store::{NoteStore, Scope, prompt_index, slug};
#[cfg(any(test, feature = "test-support"))]
pub use store::{INDEX_FILE, UNTRUSTED_INDEX_LABEL};
#[cfg(any(test, feature = "test-support"))]
pub use tools::NOTE_TOOL_NAMES;
pub use tools::{NOTE_FORGET, NOTE_LIST, NOTE_READ, NOTE_SAVE, NOTES_SOURCE, NoteToolDispatcher};

