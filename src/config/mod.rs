//! `tui.toml`: rkat-tui's own configuration file, its layering and the
//! writes to the realm's copy.
//!
//! Three files are layered: `~/.rkat/tui/tui.toml` (global, see
//! [`crate::bootstrap::GlobalDirs`]), `<realm>/tui.toml`
//! (per-workspace, the default write target) and `<project>/.rkat/tui.toml`
//! (committed with the repository, therefore untrusted: it may only add deny
//! rules and `[auto]` settings, may not loosen `trust_project_builds`, and
//! its endpoints are ignored). The realm file lives under the config
//! directory, out of the agent's shell's and a clone's reach: it is the
//! user's and applies in full. The command-line overlay ([`crate::overlay`]) is a fourth layer between the
//! realm and project files.
//!
//! Besides the approval rules (`[[allow]]`/`[[deny]]`, see
//! [`crate::approval::rules`]) and `[auto]`, the file holds
//! `[endpoints.<name>]` (see [`crate::endpoints`]),
//! `[pricing."<model id>"]` price overrides (see [`crate::pricing`]),
//! `[confinement] extra_read`/`extra_write` sandbox grants (see
//! [`crate::bootstrap::confinement`]), `[thinking]` Anthropic thinking per
//! model (see [`crate::thinking`]), `[helpers]`, `[shell]`, `[ui]`,
//! `[daemon]` (and the retired `attach`, read and ignored with a notice);
//! like endpoints and prices, thinking, confinement grants and `[daemon]`
//! are honoured from the user's own layers only.
//!
//! - [`file`](mod@file): the schema ([`RulesFile`] and its sections, the settings
//!   after layering);
//! - [`layers`](mod@layers): [`load_layered_with`] and its [`Notice`]s;
//! - [`persist`](mod@persist): the realm file's writes.

pub mod file;
pub mod layers;
pub mod persist;
pub mod settings;
#[cfg(test)]
mod tests;

pub use file::*;
pub use layers::*;
pub use persist::*;
pub use settings::ChecklistView;
