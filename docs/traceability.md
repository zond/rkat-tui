# Requirement Traceability Matrix

Updated after P6 and the review fix pass (2026-11-05). Paths are relative to the repository root.

| REQ-ID | Phase | Implemented In | Runtime Caller | Test Name | Status |
| --- | --- | --- | --- | --- | --- |
| TYPE-001 | 0 | `src/cli.rs` | `rkat-tui` main | `cli::tests::*` | TYPED |
| TYPE-002 | 0 | `src/error.rs` | `rkat-tui` main | compile-time exhaustive `exit_code()` | TYPED |
| TYPE-003 | 2 | `src/approval/{policy,rules,engine,counters,escalation}.rs` | `ApprovalGate` (admission) | `approval::policy::tests::*`, `tests/approval_policy.rs` | TYPED |
| TYPE-004 | 0/6 | `src/config/`, `src/overlay.rs`, `src/ui/compaction.rs` | `bootstrap::approval::load_layers` | `overlay::tests::*`, `config::tests::*`, `compaction::tests::*` | TYPED |
| TYPE-005 | 1 | `src/ui/delivery.rs`, `src/runner/mod.rs` | runner admission acks | `ui::tests::*steer*`, `tests/integrator.rs` | TYPED |
| CONTRACT-001 | 0 | `src/main.rs` | `meerkat_runtime::host_stack::run_host` | `rkat-tui --help` smoke | VERIFIED |
| CONTRACT-002 | 1 | `src/bootstrap/mod.rs`, `src/session/mod.rs` | `build_runtime_backed_service_with_default_reconfigure_host` | `tests/integrator.rs` | VERIFIED (no `SessionAgentBuilder` wrapper: gate installed per build, see evidence P2) |
| CONTRACT-003 | 2 | `src/approval/admission.rs` | `SessionBuildOptions.tool_dispatch_admission` | `tests/approval_service.rs`, `tests/mobs.rs` | VERIFIED |
| CONTRACT-004 | 2 | `src/approval/classifier.rs` | `ApprovalEngine` | `tests/classifier_llm.rs`, `tests/e2e_classifier.rs` (ignored, live) | VERIFIED |
| CONTRACT-005 | 1/6 | `src/bootstrap/{scope,realm_home,dirs,instructions}.rs` | `bootstrap::prepare` | `bootstrap::scope::tests::*`, `bootstrap::realm_home::tests::*`, `tests/integrator.rs::{a_moved_workspace_starts_anew_and_is_offered_its_old_state, after_a_rehome_continue_resumes_the_latest_conversation_in_the_new_place}` | VERIFIED |
| CONTRACT-006 | 3 | `src/bootstrap/confinement.rs` | `AgentFactory::with_shell_confinement`; realm `[confinement]` floor via `meerkat::HostConfinement::resolve` | `bootstrap::tests::each_confinement_mode_reaches_the_factory_shell`, `bootstrap::confinement::tests::a_realm_policy_is_the_floor_the_command_line_may_only_narrow`, `tests/confinement.rs` (sandbox probe; `SKIPPED:` without user namespaces) | PARTIAL (shell only; hooks and MCP stdio run as the trusted host, see evidence P3) |
| REQ-001 | 0 | crate, `scripts/release-rust-crates.sh`, `Cargo.toml`, `CHANGELOG.md` | release tooling | `make check-rust-release-config` | PROVISIONAL (`MODULE.bazel.lock` refresh pending) |
| REQ-002 | 1 | `src/session/mod.rs`, `src/runner/mod.rs` | `MeerkatMachine::accept_input_with_completion` | `tests/integrator.rs` | VERIFIED |
| REQ-003 | 1/2 | `src/runner/lifecycle.rs`, `src/session/mod.rs` | `SessionHandle::resume` | `tests/integrator.rs`, `tests/approval_service.rs` (transcript seed) | VERIFIED |
| REQ-004 | 4 | `src/endpoints.rs`, `src/models.rs`, `src/runner/{models,cost}.rs`, `src/workbench/{models,cost,cost_store}.rs`, `src/pricing.rs`, `src/bootstrap/realm_config.rs` | `reconfigure_session_llm_identity`; `FileConfigStore::patch` (realm document) | `tests/models.rs`, `endpoints::tests::*`, `workbench::cost_store::tests::*` | VERIFIED |
| REQ-005 | 5 | `src/mobs/*` | `MobMcpState` customizer | `tests/mobs.rs` | `delegate` and the engine's `fork_off`, both behind the gate (the TUI's own `fork` tool went with the in-process runner) (`tests/attach_approvals.rs::a_fork_off_child_runs_behind_the_gate`) |
| REQ-006 | 6 | `src/bootstrap/{instructions,mcp,persona}.rs`, `src/bootstrap/mod.rs` (`memory_probe`, `AgentFactory::memory`), `src/memory/*`, `src/ui/compaction.rs`, `src/runner/context.rs`, `src/workbench/memory.rs`, `src/session/mod.rs` | `Template::options` → `SessionBuildOptions.{additional_instructions,mcp_servers,auto_compact_threshold_override}`; `SessionServiceControlExt::append_system_context` | `tests/instructions.rs`, `tests/compact.rs`, `tests/semantic_recall.rs`, `tests/memory_tools.rs`, `tests/approval_policy.rs::mcp_tools_reach_the_classifier_unless_a_rule_decides`, `bootstrap::instructions::tests::*`, `bootstrap::mcp::tests::*`, `ui::compaction::tests::*`, `approval::policy::tests::upstream_builtins_are_all_classified` | VERIFIED (`/compact now` is the engine's `request_compaction`, no rebuild; evidence P6 gaps 1 and 3 are marked resolved and superseded) |
| E2E-001 | 0 | `src/main.rs` | `rkat-tui --help` | `cli::tests::help_renders_without_terminal`, binary smoke | VERIFIED |
| E2E-002 | 1 | `src/runner/testing.rs` | headless runner | `tests/integrator.rs` | VERIFIED |
| E2E-003 | 2 | `src/approval/*` | real session service | `tests/approval_service.rs` | VERIFIED |
