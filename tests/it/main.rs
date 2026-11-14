//! The integration tests, one binary: every former `tests/<name>.rs` is
//! the module `<name>` here, so the engine is linked into one test
//! program instead of one per file. nextest still runs each test in its
//! own process. Shared support: `common` (the scripted LLM, the
//! workspace, the command lines), `rig` (the daemon rig) and `fake` (a
//! fake OpenAI-compatible endpoint).
//!
//! Run one former file's tests with
//! `cargo nextest run --all-features --test it -E 'test(/^<name>::/)'`.

mod common;
#[path = "common/fake_openai.rs"]
mod fake;
#[path = "common/rig.rs"]
mod rig;

mod agents;
mod approval_policy;
mod approval_service;
mod approvals;
mod classifier_check;
mod classifier_llm;
mod client;
mod client_turns;
mod compact;
mod confinement;
mod conversations;
mod corpus;
mod councils;
mod cross_workspace;
mod daemon;
mod deferral;
mod drift_pins;
mod e2e_classifier;
mod file_tools;
mod helpers;
mod hook_source_trust;
mod instructions;
mod layout;
mod lifecycle;
mod litellm;
mod memory_tools;
mod mob_destroy;
mod mobs;
mod models;
mod pushes;
mod realm_trust;
mod rehome;
mod render;
mod schedules;
mod semantic_recall;
mod shell_jobs;
mod skills;
mod tool_costs;
mod usage;
mod watches;
