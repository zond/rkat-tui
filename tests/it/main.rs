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
mod approve;
mod assign;
mod btw;
mod checkpoints;
mod classifier_check;
mod classifier_llm;
mod classifier_transcript;
mod client;
mod client_turns;
mod compact;
mod compact_prepare;
mod confinement;
mod connect;
mod contacts;
mod conversations;
mod corpus;
mod councils;
mod cross_workspace;
mod daemon;
mod deferral;
mod drift_pins;
mod e2e_classifier;
mod file_tools;
mod forks;
mod helper_endings;
mod helper_models;
mod helper_names;
mod helper_rows;
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
mod peer_requests;
mod permission_sync;
mod permissions_dialog;
mod pushes;
mod realm_settings;
mod realm_trust;
mod rehome;
mod render;
mod resumed_runs;
mod schedules;
mod semantic_recall;
mod settings;
mod shell_jobs;
mod skills;
mod ssh_wires;
mod storage;
mod tool_costs;
mod trusted_hosts;
mod typed_steers;
mod upgrades;
mod usage;
mod watches;
