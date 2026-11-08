# RCT Evidence Log — rkat-tui

Evidence is appended after each phase gate: exact commands, base commit,
timestamp, and output excerpts.

## Phase 0 (P0a skeleton) — partial

- Base commit: `c89d1ccc6e` (uncommitted working tree)
- Date: 2026-11-05
- `make check-rust-release-config`: `Rust release config is valid (15 publish levels)`, crate-level tests `OK`.
- `node scripts/generate-bazel-rust-builds.mjs --check`: clean after generation.
- Not yet run (blocked): `./scripts/repo-cargo check/clippy/run -p meerkat-tui`
  (host disk full), `make buildbuddy-lock-update` (GitHub downloads returned
  504), so `make verify-bazel-locks` still reports the stale MODULE.bazel.lock.

## Phase 0 (P0b verbatim ports) — 2026-11-05

- Base commit: `c89d1ccc6e` (working tree before the P0 commit)
- Lane: `RUST_LANE_ID=tui-p0b-logic` (ports built on `tui-p0b-ui` and `tui-p0b-logic`)
- `./scripts/repo-cargo fmt -p meerkat-tui -- --check`: exit 0
- `./scripts/repo-cargo clippy -p meerkat-tui --all-targets --all-features -- -D warnings`: exit 0
- `./scripts/repo-cargo test -p meerkat-tui`: lib 349 passed / 3 ignored (live-API);
  approval_policy 15, file_tools 22, memory_tools 2, render_snapshot 11; exit 0
- `./scripts/repo-cargo run -p meerkat-tui --bin rkat-tui -- --help </dev/null`: exit 0
- `make check-rust-release-config`: OK (6 crate-level tests)
- Left for P2: `ApprovalEngine` wiring, `LlmClassifier` on `meerkat-llm-core`.
- `make verify-bazel-locks`: all five gates pass after `make buildbuddy-lock-update` and BUILD regeneration.

## Phase 1 (session on the runtime path) — 2026-11-05

- Base commit: `8a99d61a38` (P0), uncommitted working tree
- Lane: `RUST_LANE_ID=tui-p1`
- `./scripts/repo-cargo fmt -p meerkat-tui`: clean
- `./scripts/repo-cargo clippy -p meerkat-tui --all-targets --all-features -- -D warnings`: exit 0
- `./scripts/repo-cargo nextest run -p meerkat-tui`: 429 run, 429 passed, 3 skipped (live-API);
  lib 368, approval_policy 15, file_tools 22, memory_tools 2, render_snapshot 11, integrator 8
- `nextest run -p meerkat-tui --test integrator` repeated 5x: 8/8 each time (~1.3 s)
- integrator (real `PersistentSessionService` + `MeerkatMachine`, scripted client):
  turn completes (P1-T001); `Queue` prompt behind a running turn and a `Steer`
  applied at a `read_file` tool boundary (`boundary_append_applied`, one run,
  steer after the tool result) (P1-T002); interrupt returns the steer as
  undelivered and it never reaches the model; `/new` + `/resume <id>`
  in-process, `last` resolution, `-c` and `--resume <prefix>` across
  lifetimes, workspace copied to a new path still resumes (P1-T003)
- `./scripts/repo-cargo run -p meerkat-tui --bin rkat-tui -- --help </dev/null`: exit 0
- `./scripts/repo-cargo run -p meerkat-tui --bin rkat-tui -- --realm-backend memory -w <dir> </dev/null`:
  exit 2 in 0.3 s, "stdin and stdout must be a terminal…" (no bootstrap)
- `node scripts/generate-bazel-rust-builds.mjs`: `crates/meerkat-tui/BUILD.bazel` regenerated;
  the root `BUILD.bazel` also needs `//crates/meerkat-tui:integrator_test` in its
  test suite (left to the pre-commit hook; outside this phase's edit scope)

## Phase 2 (approval gate) — 2026-11-05

- Base commit: `99364f1e82` (P1), uncommitted working tree
- Lane: `RUST_LANE_ID=tui-port`
- `./scripts/repo-cargo fmt -p meerkat-tui -- --check`: exit 0
- `./scripts/repo-cargo clippy -p meerkat-tui --all-targets --all-features -- -D warnings`: exit 0
- `./scripts/repo-cargo nextest run -p meerkat-tui`: 477 run, 477 passed, 6 skipped (live-API);
  lib+bin 398, approval_policy 15, approval_service 6, classifier_llm 15, file_tools 22,
  integrator 8, memory_tools 2, render_snapshot 11 (e2e_classifier: 3 ignored, live keys)
- `nextest run -p meerkat-tui --test approval_service --test integrator` repeated 5x: 14/14 each time (~1.4 s)
- `./scripts/repo-cargo run -p meerkat-tui --bin rkat-tui -- --help </dev/null`: exit 0
- `node scripts/generate-bazel-rust-builds.mjs`: `crates/meerkat-tui/BUILD.bazel` and the root
  `BUILD.bazel` test suite gained `approval_service_test` and `classifier_llm_test`; `--check` clean
- Gate: `approval::admission::ApprovalGate` implements `meerkat_core::ToolDispatchAdmission`;
  `SessionHandle::{create,resume}` install it via `SessionBuildOptions.tool_dispatch_admission`
  (composed after any preceding admission). Deny = `ToolError::PolicyDenied { ToolConsequenceDenial
  { code: "auto_mode", message: deny_message } }`: the model reads
  `{"error":"policy_denied","message":"Tool call denied (auto_mode): Blocked by auto-mode safety policy: …"}`,
  the UI sees `ToolExecutionCompleted { is_error: true }` plus `Msg::Approval` with the reason, and the run continues.
- approval_service (real `PersistentSessionService` + `MeerkatMachine`, scripted LLM, shell builtin on):
  tier-1 `read_file` runs unreviewed; a classifier-blocked `shell touch <outside>` never runs and the
  next model request carries the reason (P2-T001); a scripted `ActionClassifier` allow runs a
  `write_file` outside the workspace (P2-T002); a classifier error escalates, answered through
  `Effect::RespondEscalation`, ledger record `approved`; an unanswered escalation times out as a
  denial and closes the dialog; `--resume` re-seeds the classifier transcript from the authoritative session.
- classifier_llm (shared scripted `LlmClient`): stage-1 parse/normalize, stage-2 verdicts,
  malformed verdict ⇒ block, bounded tool loop (6 calls / 8 round trips, final tool-less call),
  tools rejected ⇒ tool-less stage 2, 16 KiB result cap, timeouts, typed `ProviderTag` knobs, usage events.
- Gap recorded: no `SessionAgentBuilder` wrapper — `build_runtime_backed_service_*` take a concrete
  `FactoryAgentBuilder`, and `CustodyEvidenceSource`, `arm_default_runtime_delivery` and the
  reconfigure-service impl are crate-private/specialised, so a wrapped builder would lose process
  custody, durable job delivery and hot swap. Upstream ask: a generic composition or a
  `FactoryAgentBuilder::default_tool_dispatch_admission` slot.

## Phase 3 (confinement + custody) — 2026-11-05

- Base commit: `cc4d0f57db` (P2), uncommitted working tree
- Lane: `RUST_LANE_ID=tui-port`
- `bootstrap::confinement`: `--confinement` lowered into an `ExecutionConfinement` built by the TUI
  (`workspace`: rw workspace + temp dir + Cargo home + user cache dir, ro rustup home, `/usr/{local,share,include}`,
  `/opt`, `/etc/{ssl,pki,ca-certificates}` and the files `/etc/{resolv.conf,hosts,nsswitch.conf,passwd,group,gitconfig}`,
  network `unrestricted`; `strict`: same filesystem, network `denied`; `off`: `ShellConfinement::TrustedHost`), checked once
  with `meerkat_sandbox::CompiledConfinement::compile`, installed with `AgentFactory::with_shell_confinement`. A refusal is a
  start-up `TuiError::Confinement` (exit 2) carrying the upstream `ConfinementRefusal` text and naming `--no-confinement`.
  A realm `[confinement]` is ignored with a start-up notice; the mode is announced as an Info notice
  (`confinement: workspace (network allowed)`).
- Why not upstream's `workspace` preset (`HostConfinement::resolve`): its extra grants are subtrees only, and the Linux
  backend refuses grants overlapping each other or the baseline (`/etc` contains `/etc/ld.so.cache`, `/usr` contains
  `/usr/bin`), missing paths and paths through symlinks, so the preset cannot grant name resolution files. The TUI's
  `grants()` keeps each extra path only when existing, canonical and disjoint. Verified on this host: with the detected
  paths the compile now fails only at the namespace probe (`BackendUnavailable`), no longer `UnsupportedRequirement`.
- Only the shell is confined (hooks and MCP stdio stay trusted host): confining every kind triggers
  `HostConfinement::admit_host_inputs`, which refuses the TUI's default in-workspace realm state on Linux (no deny lists).
  macOS adds `deny_write <ws>/.rkat` like the upstream preset.
- Custody: already wired by `build_runtime_backed_service_with_default_reconfigure_host` (it calls
  `set_interrupted_tool_evidence_source(CustodyEvidenceSource)` when the factory has a `runtime_root`; the TUI sets it to
  the realm root, and `bind_shell_process_custody` opens `<realm>/process_custody` per session). Proven by
  `tests/confinement.rs::an_interrupted_shell_call_leaves_custody_records_and_no_process` (interrupt during `exec sleep 60`:
  the process is gone, custody records exist under the session's scope).
- Tiers unchanged; `approval/policy.rs` module docs list which Tier-3 shell risks each mode contains.
- Tests: `bootstrap::confinement::tests` (mode → requirement table, grant filtering, realm override notice, relative root),
  `bootstrap::tests::each_confinement_mode_reaches_the_factory_shell` (`AgentFactory::shell_confinement()` equals the
  lowered requirement per mode); `tests/confinement.rs` workspace write containment and strict network denial
  (`bash -c 'exec 3<>/dev/tcp/127.0.0.1/<port>'` against a test listener) — **skipped on this host** with the printed
  reason: Ubuntu's `kernel.apparmor_restrict_unprivileged_userns=1` makes the Linux backend `BackendUnavailable`
  (`unshare -Ur true` fails the same way). Not `#[ignore]`d: they run wherever the backend works.
- Gap: on this host (and stock Ubuntu 24.04+) the default `workspace` mode refuses to start; users need
  `--no-confinement` or an AppArmor profile allowing user namespaces for `rkat-tui`. Also, where `/etc/resolv.conf` is a
  symlink (systemd-resolved), it could not be granted, so DNS failed inside the Linux sandbox even in `workspace` mode.
  **Resolved on-branch** (2026-11-06): see the DNS entry below.

## Phase 4 (models, endpoints, cost) — 2026-11-05

- `endpoints.rs` full port (registry, priority, `parse_model_ref`, bare-id lookup, `plan_startup`, `restore_choice`,
  sidecar `<realm>/tui_endpoints/<session id>.toml` with per-model cost) on the typed config:
  built-ins via `resolve_auth_binding_or_default_for_provider` (realm binding on the TUI realm's chain, else env default;
  `RKAT_` prefixes and Gemini's `GOOGLE_API_KEY` fallback honoured); keys from token-store/OAuth bindings are not
  readable, so those endpoints exist but cannot be listed (picker warning).
- Dialect paths (all through an in-memory layer the factory reads via a TUI-owned `MemoryConfigStore` passed to
  `FactoryAgentBuilder::new_with_config_store`; never written to a realm document):
  built-in `anthropic`/`openai`/`gemini` → default resolution, no binding; every configured `[endpoints.<name>]`
  (OpenAI dialect incl. LiteLLM/vLLM, Anthropic- and Gemini-dialect gateways) → backend/auth/binding in the synthetic
  realm `rkat-tui-endpoints` (dialect provider, the dialect's API-key backend kind, `base_url`, `Env{api_key_env}` or
  `InlineSecret`), selected by an explicit `AuthBindingRef` on the session. The realm is on no session's chain, so
  built-in default resolution is unchanged (tested).
  `self_hosted.servers/models` not used: `SelfHostedTransport` is only `openai_compatible` (api_style
  `chat_completions|responses`), and each model must be a global pre-declared alias pinned to one server, which fits
  neither run-time model lists nor one id on two proxies; Anthropic/Gemini have no self-hosted transport.
- Models only an endpoint knows get a `[models.<id>]` (`CustomModelConfig`) in the same layer, from the listing
  (context window, max output). Nothing added to the `meerkat-models` catalog; the catalog is consulted only for
  "is there already a profile" and as the gauge's second source.
- `/model` picker fed only by live lists (`models::ModelCache`, parallel fetch via an injectable `ModelLister`, 10 s per
  request, fetched once per process in the background or at start-up for bare-id lookups, `/model refresh`).
  Switch: `Runtime::ensure_profile` → `SessionHandle::switch_model` → `MeerkatMachine::reconfigure_session_llm_identity`
  (`SessionServiceRuntimeExt`, `SessionLlmReconfigureRequest{model, provider, auth_binding: Set|Clear}`), between turns.
- Gap: an agent captures its model registry at build (`effective_model_registry`), so a model whose profile was added
  after the build is refused by the hot swap ("absent from the agent's effective model registry"). The TUI rebuilds the
  session (retire + resume) first when its profile generation is older (`Runtime::profile_generation`). Resuming a
  session switched to such a model re-adds its profile before the build, else the tool-visibility restore guard
  (`capability_base_filter_matches_current_surface`) rejects it. Upstream ask: a registry refresh on reconfigure.
- Cost: UI counts `TurnCompleted` usage under `TurnUsage::accounting().model` (fallback: status model), classifier
  usage under the classifier model; snapshots saved to the sidecar; resume loads exact totals (derived/approximate only
  for sessions with no sidecar). Prices: `[pricing]` overrides > LiteLLM `/model/info` (from the listings) > community
  file (7-day cache in the global config dir, background refresh, `--no-pricing-fetch`). Gauge: endpoint window >
  registry window > `DEFAULT_CONTEXT_WINDOW` (128k, shown `~`).
- Tests: `endpoints::tests` (ported tables: parse, lookup/tie-breaks, discover, startup plans, restore, sidecar choice +
  cost round-trips; new: lowering resolves each binding to its `base_url`/key source with built-in resolution unchanged,
  `build_llm_client_for_identity` builds each dialect's client for a configured endpoint, profiles only for unknown
  models), `bootstrap::tests::startup_model_planning`, `credentials::tests`, UI cost arithmetic
  (`session_cost_arithmetic_from_turns_and_the_classifier`, accounted-model attribution), `pricing` cache TTL tests
  (existing), `tests/models.rs`: picker → bare-id switch to a gateway-only model → next request's `model` is
  `my-alias`, session metadata carries the gateway binding, sidecar + cost survive quit/resume, switch back to
  `anthropic:claude-opus-4-8` clears the binding and the next request uses it; unknown endpoint leaves the model.
- `./scripts/repo-cargo fmt -p meerkat-tui -- --check`: clean; `clippy -p meerkat-tui --all-targets --all-features -- -D warnings`: exit 0
- `./scripts/repo-cargo nextest run -p meerkat-tui`: 496 run, 496 passed, 6 skipped (live-API);
  lib+bin 412, approval_policy 15, approval_service 6, classifier_llm 15, confinement 3 (2 skip internally, see P3),
  file_tools 22, integrator 8, memory_tools 2, models 2, render_snapshot 11
- `--test models --test confinement --test integrator --test approval_service` repeated 5x: 19/19 each time (~2 s)
- `rkat-tui --help </dev/null`: exit 0; `rkat-tui --realm-backend memory --no-confinement -w <dir> </dev/null`: exit 2 in
  <0.01 s ("stdin and stdout must be a terminal"); in a pty with a fake key: default mode exits 2 with the confinement
  refusal; `--no-confinement` runs, `/help`, `/model`, `/cost`, `/quit` → exit 0, no panic.
- `node scripts/generate-bazel-rust-builds.mjs`: `confinement_test` and `models_test` added to
  `crates/meerkat-tui/BUILD.bazel` and the root `BUILD.bazel` test suite; `--check` clean.
- `Cargo.toml`: `meerkat-sandbox` left the machete `ignored` list (used by `bootstrap::confinement::check`).

## P5 — Mobs (helpers as mob members)

- Hosting (`src/mobs/`, feature `mob`, default on): `MobMcpState::new_with_runtime_adapter(service, Some(machine), Owner)`
  over the TUI's own `PersistentSessionService` + `MeerkatMachine`, `.with_persistent_storage_root(Some(<realm>))`
  (durable mob state under the realm, as `rkat` does; cheap: one SQLite profile store), `.with_workgraph_service`
  (realm-scoped `WorkGraphService` from the persistence bundle; without it a member build that resolves WorkGraph on
  fails "WorkGraph tools enabled without a host-issued namespace grant"), `.with_default_llm_client(llm_override)`,
  `.with_child_spawn_member_customizer(HelperCustomizer)`. `AgentMobToolSurfaceFactory` goes into the builder's
  `default_mob_tools` slot (taken before the builder is consumed), so members get mob tools too (nesting per upstream
  defaults). `AgentFactory::mob(cfg!(feature = "mob"))`; every session build (create and resume) sets
  `apply_generated_create_only_mob_operator_access(Enable)` (resumed metadata alone never re-enables it).
  Member usage is persisted in the TUI's cost sidecar, so finished helpers stay accounted on resume regardless of mob
  storage.
- Gate inheritance (no fake): `SpawnMemberCustomizer::customize_spawn` (asked for every member of every mob the state
  creates or restores, fresh spawn / restore / explicit resume) sets `spec.tool_dispatch_admission =
  ApprovalGate::for_helper(engine, owner, label).after(previous)`. Owner = the session whose implicit mob
  (`implicit-<session id>`) the helper runs in; a helper of a helper inherits its owner via the registry (helper
  session → tag, bound by the follower); other mobs (`mob_create`) → most recently opened TUI session; no session →
  spawn refused (fail closed). `ApprovalEngine::decide_as(Caller::helper(owner, label), …)`: the owner's transcript
  (user words only; the helper's task appears solely as the parent's `delegate` action, never as `[user]`), the
  owner's allow cache and Stop, the shared denial counters; `EscalationPrompt.agent` → modal line `Helper: <label>`;
  `ApprovalEvent::Decision.agent` → notices `[label] tool`.
- Policy: `delegate`/`fork_off` are `ActionKind::Delegation` → Tier 1 (delegating does nothing by itself; every helper
  call passes the gate). Read-only mob queries (`mob_list*`, `mob_check_member`, `mob_profile_get/list/list_sources`)
  joined `READ_ONLY_BUILTINS`; mutating `mob_*` stay Tier 3.
- Model/binding: a `delegate` helper runs on its delegator's model (upstream implicit-mob profile); the customizer adds
  the owner's `auth_binding` when the helper's model equals the owner's (an explicit `model_override`/inline profile
  model keeps default resolution). Sessions register model+binding with the registry on open and on `/model` switch.
- Helpers get the TUI's file tools (`spec.external_tools`, fresh `FileToolDispatcher`, no diff channel) and
  `HELPER_PERSONA` (`spec.additional_instructions`). `InheritParent` tooling needs provenance witnesses for the parent's
  external tools: file tools and notes now carry `ToolProvenance(Callback, "rkat-tui.files" / "rkat-tui.notes")`.
- Observation: the customizer announces each spawn; `follow::watch` subscribes via
  `handle.subscribe_agent_events_from(identity, SessionEventCursor::Earliest)` (retrying while the mob/member is not
  registered), binds the helper session (`resolve_bridge_session_id`, envelope source), forwards events scoped as
  `[Primary{owner}, MobMember{label}…]`, and ends when the member leaves the roster (a retired member's session stream
  does not end by itself; roster checked every 200 ms). Runner forwards `Msg::Scoped` and
  `StatusUpdate::Helpers(Vec<HelperStatus>)`; status bar shows `N helper(s)`; transcript blocks `[label · model] tool`
  with depth = number of member frames.
- Cost: no `usage_total` exists for delegate helpers (that projection is flow runs only, `meerkat-contracts` wire
  `MobRun`); per-helper usage comes from the helpers' own `TurnCompleted` (`TurnUsage` accounting model) →
  `SessionCost::record_helper` (adds to `per_model` — footer total — and `helpers[label][model]`); `/cost` prints
  "of which helpers:" lines; sidecar `[helper_cost.<label>."<model>"]` (serde-default, old sidecars load).
- Interrupt: `SessionHandle::interrupt` → `Helpers::cancel_for(owner)`: `force_cancel_member` then `retire` for each
  live helper of the session (the abandoned `delegate` call would otherwise leave the member seated), then the existing
  run stop; `engine.stop(owner)` also denies helpers' reviewed calls.
- Gaps / upstream asks:
  1. `MobDefinition::implicit(session, model)` carries no provider/self-hosted id and `validate` accepts only catalogued
     or `[models.<id>]`-in-definition models, so a session on an endpoint-only model id (e.g. `proxy:my-alias`) cannot
     `delegate` ("unknown_model: profiles.delegate model … neither catalogued …", returned to the model as the tool
     error). Ask: let the implicit mob inherit the delegator's provider / custom model profile. **Resolved on-branch**
     (2026-11-06, engine change in `meerkat-mob-mcp`; narrowed for review the same day):
     `MobMcpState::with_implicit_mob_customizer` takes an `ImplicitMobCustomizer`, asked once with an `ImplicitMobContext` (owner bridge session, model) for
     `delegate_provider` before the create; the engine sets `profiles.delegate.provider` from a `Some` answer and the
     host never touches the definition (`implicit_mob::implicit_mob_definition`). Opt-in; no other surface installs
     one. The TUI's `mobs::endpoint_models::EndpointModels` answers the provider of the realm's `[models.<id>]` profile
     (read from `Runtime::effective`); the member build reads that profile from the realm config. Unit:
     `implicit_mob::tests::*`, `tests::implicit_mob_for_an_uncatalogued_model_is_refused_without_a_customizer`,
     `tests::implicit_mob_without_a_customizer_is_exactly_the_implicit_definition`,
     `tests::implicit_mob_customizer_provider_makes_an_uncatalogued_model_validate_and_is_reused`,
     `tests::implicit_mob_customizer_error_refuses_the_create`; TUI:
     `mobs::a_session_on_an_endpoint_only_model_delegates_on_it`. **Superseded upstream** (2026-11-07): upstream main hands the mob tools the provider route the
     factory resolved and `delegate` creates the implicit mob on it (`ensure_implicit_mob_on_route`); the customizer
     and `EndpointModels` are gone. `fork` still calls `ensure_implicit_mob_for_model` (the route-aware variant is
     `pub(crate)`), so on an uncatalogued model it fails with `ForkError::UncataloguedModel` unless `delegate` created
     the mob first. Found with it: the agent's mob tools capture the
     model at build (`AgentMobToolSurface.model`), so after a hot `/model` switch `delegate` helpers ran on the old
     model. The TUI now rebuilds the session after every switch
     (`RebuildReason::ModelSwitch`); `mobs::a_helper_delegated_after_a_model_switch_runs_on_the_new_model`. Ask: the
     mob tools read the session's current model.
  2. `fork_off` requires the caller to be a durable mob member; the TUI's primary session is not one and delegate
     helpers are `durable: false`, so only `delegate` (and the `mob_*` tools) are usable from the TUI.
  3. Hook inheritance for members: not used (the TUI gates via admission, hooks only observe); not verified.
  4. A retired member's session event stream stays open; the follower polls the roster. Ask: end member subscriptions
     on retire.
- Tests: `tests/mobs.rs` (real service, scripted client): `a_delegated_helper_runs_behind_the_sessions_gate` (delegate
  → helper one turn on the session's model with helper persona and file tools; helper `read_file` allowed; helper
  `shell` Tier 3 denied by the classifier with the reason in the helper's tool result; classifier saw `USER-ASK`, no
  `[user]` line with the task; denial counted on the parent's counters; notice `[scout] shell`; parent received
  `HELPER-DONE`; `Helpers` 1 → 0; scoped events depth 1; App cost `helpers.scout` = 20 in / 10 out, rolled into
  `per_model`, breakdown line; sidecar stores it), `an_interrupt_leaves_no_running_helper` (helper hangs, Esc → turn
  interrupted, `Helpers([])`, `helpers.live()` empty, implicit mob roster empty),
  `a_configured_endpoints_helper_uses_the_same_binding` (`--model proxy:claude-sonnet-4-6`: helper request model and
  helper session metadata model + `rkat-tui-endpoints/proxy` binding). Unit: registry owners/nesting/live, scoped path,
  transcript nested labels with model, cost roll-up/breakdown, policy delegation tier, personas, render snapshot shows
  `Helper: researcher`.
- `./scripts/repo-cargo fmt -p meerkat-tui -- --check`: clean; `clippy -p meerkat-tui --all-targets --all-features -- -D
  warnings`: exit 0 (lint enforcement probed with a deliberate `unwrap`).
- `./scripts/repo-cargo nextest run -p meerkat-tui`: 504 run, 504 passed, 6 skipped; lib+bin 417, approval_policy 15,
  approval_service 6, classifier_llm 15, confinement 3, file_tools 22, integrator 8, memory_tools 2, mobs 3, models 2,
  render_snapshot 11. `--test mobs --test approval_service --test integrator --test models` ×5: 19/19 each.
- `rkat-tui --help`: exit 0; pty smoke `--no-confinement` (fake key, DSR answered) `/help`, `/cost`, `/quit` → exit 0, no
  panic.
- `node scripts/generate-bazel-rust-builds.mjs`: `mobs_test` added to `crates/meerkat-tui/BUILD.bazel` and the root
  `BUILD.bazel`; `--check` clean. `Cargo.toml`: `meerkat-mob`, `meerkat-mob-mcp` left the machete `ignored` list.

## P6 — Context, memory, compaction policy, docs (2026-11-05)

- Base commit: `568b33af83` (P5), uncommitted working tree; lane `RUST_LANE_ID=tui-port`.
- Instruction files (`src/bootstrap/instructions.rs`): `InstructionSources { workspace, global, home }` discovers
  user-global `AGENTS.md` → ancestor `AGENTS.md`/`.rkat/AGENTS.md` from the git root (nearest `.git` dir or file; the
  walk stops below `$HOME`, so a dotfiles repo in home never adopts `~/AGENTS.md`) down to the workspace's parent →
  workspace `AGENTS.local.md`/`.rkat/AGENTS.local.md`; per file `normalize_agents_md_content` (upstream's
  `AGENTS_MD_MAX_BYTES` cap, truncation marker), total 96 KiB, empty/unreadable skipped; rendered
  `# Instructions (from <path>)` into `SessionBuildOptions.additional_instructions` after `CODING_PERSONA`, notes index
  last. Re-discovered by `Template::options` on every create/resume. The workspace `AGENTS.md` stays native (factory
  context root). Upstream's `find_project_agents_md_in` is no longer public (prompt assembly's
  `load_project_agents_md_in` is private), so the TUI mirrors its two candidates. `apply_system_md` sets
  `config.agent.system_prompt_file` on the factory's effective config when the realm sets neither prompt field.
- `/remember` / `#text` (`src/runner/context.rs`): `append_remember_line` on `remember_target(workspace)` (header on
  create), then `SessionHandle::append_system_context` → `SessionServiceControlExt::append_system_context`
  (`AppendSystemContextRequest::from_text("# Remembered instruction\n\n<line>")`, source `rkat-tui`, idempotency key
  `rkat-tui:remember:<hash of line>` per session); Info notice "… fully applied (as part of AGENTS.md) from the next
  session", Warn notice when the live append is refused (file still written).
- Native memory: `bootstrap::memory_probe` opens `HnswMemoryStore` at `<store_path>/memory` once (skipped for the
  memory backend); success → `AgentFactory::memory(true)` (`memory_search` composed by the factory, compaction indexes
  discarded history); failure → memory off + start-up notice (`Runtime::notices`). `HnswMemoryStore` is SQLite-backed
  now (no single-writer lock), so a second process no longer fails the open; the probe still catches a damaged or
  unwritable index, which would otherwise fail every build closed (`CapabilityUnavailable`).
- Read-only tiers: `READ_ONLY_BUILTINS` gains `blob_inspect`, `peers`; drops the stale v0.4 names `agent_status`,
  `agent_list` (no upstream tool has them; keeping them would make a same-named MCP tool Tier 1). New
  `REVIEWED_BUILTINS` names the acting builtins (shell family, `apply_patch`, `blob_save_file`, `blob_load_file`,
  `skill_invoke_function`, `web_search`, `brain_swap`, `generate_image`, comms sends, `council`,
  `conclude_objective`). `approval::policy::tests::upstream_builtins_are_all_classified` pins every name of upstream's
  `SHELL/COMMS/UTILITY/SKILL/WEB_SEARCH/BRAIN_SWAP_TOOL_NAMES` and `image_generation::TOOL_NAMES` into exactly one list
  and their tiers; `tests/semantic_recall.rs::every_offered_builtin_is_classified_and_memory_search_is_tier_one` checks
  every tool a real session offers the model (found `council` and `conclude_objective`, now classified).
- MCP (`src/bootstrap/mcp.rs`): `McpConfig::effective_servers_from_roots(config.tools.mcp_servers, workspace,
  user_config_root)` loaded once at start-up → `Runtime::mcp_servers` → every build's `SessionBuildOptions::mcp_servers`
  (factory session-owned router; `[MCP_PENDING]` native); `--wait-for-mcp` → `FactoryAgentBuilder::wait_for_mcp`.
  Project-scoped servers named in a start-up notice. No `HostConfinement` trust predicate (the TUI confines only the
  shell, P3), so a project `mcp.toml` is trusted like `rkat run` without confinement; the approval policy's protected
  `.rkat/` paths keep the model from editing it. MCP tools: `ActionKind::Other` → classifier unless a rule decides
  (`tests/approval_policy.rs::mcp_tools_reach_the_classifier_unless_a_rule_decides`).
- Compaction (`src/ui/compaction.rs`, `src/runner/context.rs`): mechanism = per-build
  `SessionBuildOptions::auto_compact_threshold_override`. `warn` → `NonZeroU64::MAX` (size trigger off; upstream still
  compacts on byte-cap/context-refusal recovery), gauge warns at `compact_warn_percent` once and every turn from
  `compact_auto_percent`; `auto` → `compact_auto_percent × window` (window: endpoint list > registry > 128k default, the
  gauge's number). The handle remembers its threshold; `ensure_threshold` rebuilds the idle session (retire + resume)
  after a model switch, a resume onto another model or a model-list fetch that moves it. `/compact` reports the policy;
  `/compact now` (idle, after ≥1 turn) retires and resumes the session with threshold `NonZeroU64::MIN` for one turn
  (the reconfigure host has no threshold field, so no live change), `Msg::CompactArmed`; `finish_turn` rebuilds it on the
  policy threshold after that turn. Not "half the context": a rebuilt agent restarts with `last_input_tokens = 0`
  (observed `CompactionStarted { input_tokens: 0 }`), so only a history/forecast threshold below the transcript fires.
  Gauge: `CompactionCompleted.estimated_history_tokens_after` (fallback `summary_tokens`), warning re-armed.
- Gaps / upstream asks:
  1. **On-demand compaction API** (ask): no `SessionService`/runtime call compacts now; `/compact now` costs two
     rebuilds and, with a one-token threshold, a long tool loop in the armed turn compacts again after
     `min_turns_between_compactions` (3) boundaries. Ask: `compact_session(id)` or a one-shot "compact before the next
     model call" flag on the runtime.
  2. `auto_compact_threshold_override` is not live-reconfigurable and is dropped by `session_recovery` (recovered builds
     set it `None`), so a session the runtime rebuilds on its own falls back to upstream's model-aware default.
  3. With a `DurableStaged` memory store (Hnsw), `CompactionCompleted` is published only when the runtime finalizes the
     memory projection at the session's next run (`finalize_committed_compaction_projections` via
     `default_event_tx`), and after a retire-and-resume between the two it never reached the session event stream in the
     tests. The compaction itself, the summary and the indexing work (asserted on the requests and `memory_search`).
  4. A resumed session keeps its persisted system prompt (factory: bare resume does not reassemble), so edited instruction
     files apply to new sessions; `/remember` covers the live session through system context.
  5. The admission gate sees only the tool name and arguments (`ToolDispatchContext` carries no provenance), so an MCP
     tool that shares a read-only builtin's name would be Tier 1. Ask: provenance in the dispatch context.
  6. `MODULE.bazel.lock` records the hash of `crates/meerkat-tui/Cargo.toml` (stale since P0; P6 changed the manifest
     again): `make buildbuddy-lock-update` needed before CI (outside this phase's edit scope).
- Machete: `meerkat-memory` now used (probe); `meerkat-hooks` and `meerkat-session` were never used (hooks only observe;
  compaction is native) and are removed from `[dependencies]`; the `[package.metadata.cargo-machete]` section is gone.
  Side effects: `Cargo.lock` (meerkat-tui's dependency list) and the generated `crates/meerkat-session/BUILD.bazel`
  visibility (drops `//crates/meerkat-tui`) change.
- Tests: `tests/instructions.rs` (SYSTEM.md replaces the base prompt; order SYSTEM.md < workspace AGENTS.md < persona <
  global < ancestor (via a `.git` file) < local; 32 KiB truncation marker; `/remember` twice → two file lines, one system
  message, next session's prompt carries the line from AGENTS.md), `tests/compact.rs` (auto: native compaction at the
  threshold, summary call, compacted request, `CompactionCompleted.estimated_history_tokens_after < 60k` on the next run;
  warn: no compaction past the threshold, the policy's gauge warns, `/compact now` arms → next turn compacts → disarmed,
  no second compaction), `tests/semantic_recall.rs` (offered tools classified; a fact compacted away is returned by
  `memory_search`), `tests/approval_policy.rs` MCP test; unit: instructions (order, `.rkat` local, truncation, total
  cap, `.git` file, home never root, empty skipped, SYSTEM.md rules, remember target/append), mcp loader, compaction
  policy/gauge/describe, UI `/compact` + `/compact now` flows, remember key.
- `./scripts/repo-cargo fmt -p meerkat-tui -- --check`: clean; `clippy -p meerkat-tui --all-targets --all-features -- -D
  warnings`: exit 0.
- `./scripts/repo-cargo nextest run -p meerkat-tui`: 522 run, 522 passed, 6 skipped; lib+bin 428, approval_policy 16,
  approval_service 6, classifier_llm 15, compact 2, confinement 3, file_tools 22, instructions 2, integrator 8,
  memory_tools 2, mobs 3, models 2, render_snapshot 11, semantic_recall 2. `--test compact --test instructions --test
  semantic_recall --test integrator --test mobs` ×5: 17/17 each.
- `rkat-tui --help`: exit 0. PTY smoke (`--no-confinement`, fake key, 160×60): `/help`, `#note from hash`, `/remember x`,
  `/compact`, `/compact now` ("nothing to compact yet"), `/quit` → exit 0, no panic, workspace `AGENTS.md` =
  `# AGENTS.md\n\n- note from hash\n- x\n`.
- `node scripts/generate-bazel-rust-builds.mjs`: `compact_test`, `instructions_test`, `semantic_recall_test` added to
  `crates/meerkat-tui/BUILD.bazel` and the root `BUILD.bazel`; `--check` clean. `make docs-check`: OK (covers the
  Mintlify docs only; the README is not under `docs/`). `make check-published-crates-standalone-guard`: meerkat-tui OK
  (no `standalone-tests` metadata: plain `cargo test`; README prints the ignored live-lane commands).
- README: `crates/meerkat-tui/README.md` rewritten for the port. CHANGELOG `[Unreleased]` `### Added` entry completed.

## Review fixes — Agent A (approval, security, helpers)

### Why not the native operation review (A22, A23)

Checked concretely against `crates/meerkat-core/src/approval/{review,consent}.rs`,
`crates/meerkat/src/operation_reviewer.rs` and `crates/meerkat/src/governed_realm/stock/`:

- The pieces are public: `OperationReviewer` (review.rs:291), `BoundOperationReview::new`
  + `.with_human_consent(route, validity)` (review.rs:552-659), `HumanConsentRoute`
  (consent.rs:262-319), `AgentFactory::with_operation_review` (factory.rs:4718).
- But review only runs for a call carrying a `WorkAuthorizationContext`: the fenced dispatch
  in `agent.rs:3646-3716` builds a review binding only `if let Some(work) =
  context.work_authorization()`, else "Ungoverned work retains no review" (3678-3683). That
  context comes only from the native grant host (`NativeGrantWorkConfiguration`,
  `meerkat-runtime/src/meerkat_machine/local_authorization.rs:141-195`), which
  `open_realm_persistence_in` (what rkat-tui opens) never installs. In an ungoverned realm a
  bound review is accepted and never invoked.
- The tier (R1/R2/R3, authorization.rs:997-1004) is authorization truth from an
  `OperationPolicyOwner`; there is no host-local "tool call → tier" policy. Stock governance
  leaves builtins unmapped: `stock/policy.rs:52-60, 746-756` refuse shell / write_file /
  apply_patch as Unavailable (no `ToolOperationTarget` for builtins), and
  `StockReviewTarget` has no builtin selector (stock/review.rs:121-151).
- The consent route needs an `AuthenticatedApprover` (Human principal in a trust domain,
  consent.rs:126-145) and settles the call as `HumanConsentRequired` for a fresh attempt
  (review.rs:1372-1437); rkat-tui's dialog answers inside the waiting call.
- `ReviewCandidate` gives no transcript (review.rs:22-27) and `ModelOperationReviewer` is a
  single tool-less request (operation_reviewer.rs:127-133), so the classifier's reasoned
  stage with read-only project tools has no native host.

So rkat-tui keeps its gate on `ToolDispatchAdmission` (the seam that runs for every call in an
ungoverned realm) and records its escalations in the realm's canonical approval store.

Upstream asks:
- (Z-H1, D#1) Host-local review tier policy for non-governed realms: a `tool call →
  OperationReviewTier` policy installable on the factory/session build without
  `NativeGrantWorkConfiguration`, plus `ToolOperationTarget::BuiltinOperation` targets and a
  stock mapping / `StockReviewTarget` selector for builtins (shell, write_file, apply_patch).
- (D#19) A typed host reviewer so the classifier's own agent loop can go away: a core-hosted
  bounded multi-turn reviewer with host-supplied read-only tools and a sanctioned
  transcript/environment context source, and a synchronous in-call `HumanConsentRoute` mode
  for single-user hosts without a trust domain.
- (D#6) Typed authorship on host prompts: a prompt admitted through `PromptInput` should carry
  `TranscriptTurnInput::TypedText` (or `RenderClass::UserPrompt`) so a resumed transcript can
  tell the human's words from peer / runtime input without a host sidecar; rkat-tui demotes
  user-role messages without typed authorship to `[context]` lines.
- (D#5) The admission seam receives no tool provenance or mutation class; rkat-tui reads the
  session's published visible tool definitions per call. Ask: pass the resolved
  `ToolDef`/`ToolProvenance` and `tool_mutation_class` of the owning dispatcher into
  `ToolDispatchAdmission::await_dispatch_admission`.

## Review fixes — Agent B (runtime, endpoints, structure, docs)

Paths under `crates/meerkat-tui/`.

- B1 durable endpoints: `src/endpoints.rs` (`endpoint_section`, `endpoint_patch`, `project_endpoints`,
  `model_profile_entry`, `ensure_model_profile`, `EndpointError::DialectConflict`) writes the `[realm.rkat-tui-endpoints]`
  backend/auth/binding entries and `[models.<id>]` profiles into the realm's own `config.toml` through
  `FileConfigStore::patch` (only changed entries; an inline key makes the document 0600). The factory reads that document
  on every build through `src/bootstrap/realm_config.rs` (`RealmConfigStore`: document + env overrides + `SYSTEM.md`),
  replacing the in-memory synthetic realm. The endpoint-choice sidecar is gone: `endpoints::endpoint_of` derives the
  endpoint from the session's persisted binding/provider. Cost records moved to `src/runner/cost_store.rs`
  (`<realm>/tui_cost/<id>.toml`, `paths::write_atomic` 0600, corrupt file renamed `.corrupt-<unix s>`, legacy
  `tui_endpoints/` totals still read). `endpoints::load_cost` stays as a re-export for `tests/mobs.rs`.
- B2: `SessionHandle` keeps no model/provider/binding; `SessionHandle::identity` reads the authoritative metadata,
  the runner re-reads it after open/switch/rebuild. `ModelChoice.endpoint: EndpointName`,
  `Runner.current_endpoint: Option<EndpointName>`. Every build carries
  `tool_access_policy = DenyList{brain_swap, apply_patch}` (`session::TUI_OWNED_BUILTINS`).
- B3: prefix inference removed; `WireDialect::of_catalog_model` = `meerkat_models::infer_provider` (catalog-exact);
  bare ids outside the catalog resolve only via live lists (or the only endpoint). `Dialect` → `WireDialect`.
- B4: `bootstrap/confinement.rs` — a realm `[confinement]` shell requirement is the floor (lowered with
  `meerkat::HostConfinement::resolve`; `strict` narrows network), `--no-confinement` against it is
  `ConfinementError::BelowRealmFloor`. `tests/confinement.rs` probes `meerkat_sandbox::prepare` + a confined `/bin/true`
  and prints `SKIPPED:`; no output-text matching.
- B5: `ui::compaction::CompactionTrigger::{Inherit, Disable, At}`; window from the model profile only
  (`runner::models::window_of` over `Runtime.effective`), listings refresh the profile; `Msg::CompactRebuilt` /
  `COMPACT_REBUILT` ("rebuilt the session so its next model call compacts"); every retire+resume is
  `Runner::rebuild_session_workaround(RebuildReason)`; background fetches never rebuild.
- B6: runner — delivery only by `BoundaryAppendApplied` or the input's completion; runs tracked in a
  `HashSet<RunId>`; typed `ui::TurnFailure::{Failed, Interrupted, Unknown, Pending}` (interrupted only on `Cancelled`
  or metadata outcome `Cancelled`); `Completion::{Pending, TerminalAtAdmission (→ Unknown), AlreadyTerminal}`; waiters
  and interrupt tasks in a `JoinSet` aborted on switch/quit; the scoped channel is gone; transcript seeding and helper
  registration happen after a successful open; `SessionHandle::close` (→ `ApprovalEngine::forget_session`) on `/new`,
  `/resume`, quit; inputs recorded with `record_user_input`/`forget_user_input` by idempotency key after admission.
  A prompt during `/compact now` goes out as an ordinary `StartTurn` (Queue) served after the rebuild.
- B7: `ui::cost::CostConfidence::{Exact, Approximate, Unmeasured}`, `SessionCost::reconcile` against
  `Session::total_usage` on resume, `record_compaction` (estimated input + summary), `Msg::HelperUnmeasured`.
- B8: typed errors with sources: `EndpointError`, `EndpointSkip`, `AdmissionError`, `ConfinementError`,
  `PricingError` (sources), `ListModelsError::{Read, Decode, Utf8, Task}`, `CostStoreError`, `TuiError::ToolGateway`,
  `TuiError::McpConfig(EffectiveMcpServersError)`; `Provider`/`MissingApiKey`/`Mcp`/`InputRejected` string variants gone.
- B9: modules are crate-private, public under the `test-support` feature (`internal_modules!` in `src/lib.rs`), plus
  `meerkat_tui::harness`; public API `run`, `Args`, `TuiError` and its error types. Self dev-dependency with
  `test-support`, `[package.metadata.meerkat.standalone-tests] features = ["test-support"]`, README prints
  `cargo test --features test-support`.
- B10: price cache in `GlobalDirs::cache` (`--config-dir/cache`, `<user-root>/.cache/rkat-tui`, `$XDG_CACHE_HOME`,
  platform cache dir).
- B11: persona trimmed (no tool contracts / interrupt semantics); notes index last, via A's labelled untrusted
  `prompt_index`. README updated for the whole pass.
- B12: bidi controls replaced in `ui::markdown::sanitize`; model-list bodies through `http::read_capped`
  (`MAX_LIST_BYTES`); project `.rkat/mcp.toml` servers only when `ProjectMcpTrust` (A's `HostTrustRecord` digest via
  `/mcp trust`, or `--allow-project-mcp`), else upstream's trusting loader refuses them and a notice lists them.
- B13: keys are `SecretString` end to end (`ProviderCredentials`, `BuiltinCredentials`), headers marked sensitive.
- B14: resumed builds pass the session's persisted `build_state.additional_instructions` (the factory never re-renders
  a resumed prompt): persona once, edited files apply from the next new session (`tests/instructions.rs`).
- B15: `/remember` key = SHA-256 of the line (pinned in a test).
- B16: REQ-006 downgraded to PARTIAL (gap 3 below stays open).

Upstream asks (one line each):
- (D#13/D#12) A typed "no size-triggered compaction" for `auto_compact_threshold_override` (today `NonZeroU64::MAX`).
- (Z-H5, D#14) An on-demand compaction call and a live compaction-trigger update on a session, so
  `rebuild_session_workaround` can go away; `CompactionCompleted` delivered after a retire-and-resume (P6 gap 3).
- (Z-M9) A runtime/provider-owned "list models for this binding" seam so the TUI's live list client can go away.
- (D#15) A host-owned secret store write path for endpoint keys (`CredentialSourceSpec::ManagedStore` from a host) so an
  inline `api_key` need not be written into the realm document.
- (B7) `CompactionCompleted` reporting the summary call's usage (today the input is the history estimate).
- (B7, A) `HelperUpdate` has no "follow failed" variant yet: `Msg::HelperUnmeasured` exists on the UI side, but nothing
  emits it until `src/mobs/follow.rs` reports a follow failure (Agent A's file).

Verification (Agent B, lane `tui-port`, 2026-11-05, concurrent with Agent A's edits):
- `fmt -p meerkat-tui`: clean. `node scripts/generate-bazel-rust-builds.mjs`: regenerated (`test-support` feature on
  the test targets, self dev-dependency); `--check` clean. `make check-published-crates-standalone-guard`: meerkat-tui OK.
- `clippy -p meerkat-tui --all-targets --all-features -- -D warnings`: no finding in Agent B's files; the remaining
  findings are in Agent A's in-progress files (`approval/engine.rs` double_must_use, `memory/store.rs` redundant_clone,
  `tests/drift_pins.rs`, `tests/mobs.rs`).
- `nextest run -p meerkat-tui --lib --test integrator --test models --test compact --test instructions --test
  semantic_recall --test confinement --test render_snapshot`: 495 passed; failing: `approval::policy::tests::
  user_rules_take_precedence` (A), and `integrator::a_steered_message_is_applied_at_the_next_tool_boundary` +
  `semantic_recall::a_compacted_fact_is_found_with_memory_search`, both because `read_file` / `memory_search` now reach
  the classifier (A4 provenance lookup; the test fails the same with B's tool policy removed). The confinement cases
  printed `SKIPPED:` (no unprivileged user namespaces on this host); the realm-floor case ran and passed.
- `rkat-tui --help`: exit 0 (`--allow-project-mcp` listed).

Verification (Agent A, lane `tui-port`, 2026-11-05): `repo-cargo fmt -p meerkat-tui -- --check` clean;
`repo-cargo clippy -p meerkat-tui --all-targets --all-features -- -D warnings` clean;
`repo-cargo nextest run -p meerkat-tui`: 588 tests run, 588 passed, 6 skipped (no hangs);
`repo-cargo run -p meerkat-tui --bin rkat-tui -- --help </dev/null` prints usage.
The A4 regression (Tier-1 `read_file`/`memory_search` reaching the classifier) was the missing
`engine.set_tool_origins(ServiceToolOrigins::new(&service))` call in `bootstrap/mod.rs`, now wired.

## User follow-ups — confinement refusal, sandbox grants, config dir, env-only endpoint keys (2026-11-05)

- Educational refusal: `ConfinementError::Unenforceable` now carries `status: HostUsernsStatus` and `help`
  (`unenforceable_help`): what blocked it (read from `/proc/sys/user/max_user_namespaces`,
  `/proc/sys/kernel/unprivileged_userns_clone`, `/proc/sys/kernel/apparmor_restrict_unprivileged_userns`, in that
  order; else the backend's refusal; macOS Seatbelt / no backend elsewhere), how to fix it (sysctl + `/etc/sysctl.d/`
  drop-in; for AppArmor also a per-binary profile with `userns,` and `apparmor_parser -r`, with the trade-off), and how
  to run unconfined (`--no-confinement`/`--confinement=off`, or the realm config when a `[confinement]` floor applies).
  Fail closed, exit 2. Unit tests: `userns_status_is_read_from_the_proc_switches` (fake proc root),
  `the_refusal_explains_the_blocker_the_fix_and_the_way_out`.
- Linux baseline (read from `meerkat-sandbox/src/linux.rs`): dirs `/bin /sbin /usr/bin /usr/sbin /lib /lib64 /usr/lib
  /usr/lib64`, files `/etc/ld.so.cache /dev/null /dev/random /dev/urandom`; empty private mount view + chroot, no
  `/etc`, `/run`, `/proc`, `/sys`; grants bound at their own path; single regular files (nlink 1) are grantable as
  `Literal`; symlinked paths and overlaps refused; seccomp admits only `AF_INET`/`AF_INET6` sockets (no `AF_UNIX`).
- DNS: `/etc/{host.conf,gai.conf,services,protocols}` added as file grants. A symlinked `/etc/resolv.conf`
  (systemd-resolved) could not be granted, and granting its target's parent (`/run/systemd/resolve`) would not help
  (glibc reads only `/etc/resolv.conf`; nss-resolve needs `AF_UNIX`). Upstream ask: the Linux backend binding a
  symlink's target at the link's path. **Resolved on-branch** (2026-11-06, the branch's first engine change):
  `meerkat-sandbox`'s Linux `Mount::capture` accepts a read-only file grant whose final component is a symlink to a
  regular file (ancestors real directories) and binds the pinned canonical target at the link's path; `validate()`
  still re-resolves the link to the pinned target, so a retargeted link fails closed. Narrowed for review: the link
  is accepted only when the link and every directory from `/` to its parent are owned by root or the euid and none is
  group/other-writable unless sticky (`link_is_trusted`); `meerkat_sandbox::literal_file_grantable` exports the
  backend's rule and the TUI's `canonical` calls it. Upstream's version (PR #2084) keeps the rule private, so since 2026-11-07 the TUI's
  `file_grantable` mirrors it. Unit:
  `linux::tests::a_final_component_link_grants_its_regular_file_target_read_only`,
  `linux::tests::a_final_component_link_in_a_shared_writable_directory_is_refused`,
  `linux::tests::a_final_component_link_owned_by_another_user_is_refused` (root only); positive lane:
  `linux_file_grant_through_a_final_symlink_binds_its_target_at_the_link`. The TUI now grants the
  link only (`canonical` admits a final file link on Linux; the target's directory is not granted); the
  `resolv_conf_link` notice is gone, and
  `workspace_mode_resolves_localhost_and_dns_names` no longer skips DNS for a link.
- Extra grants: `--confinement-read`/`--confinement-write <dir>` (`ExtraGrants`, validated by `grants_with`: absolute,
  existing, canonical, directory, off baseline/`/proc`/`/sys`/`/dev`, disjoint from the workspace and each other,
  not redundant; typed `ConfinementError::ExtraGrant`); refused under a realm floor. `ConfinementSection`
  (`extra_read`/`extra_write`) is defined but not yet a `tui.toml` key (needs a field in `approval/rules.rs`).
- Config dir: `~/.rkat/tui` (`<--user-config-root>/.rkat/tui`), `--config-dir` overrides; an existing legacy
  `~/.config/rkat-tui` / `$XDG_CONFIG_HOME/rkat-tui` is used while the new dir is missing, with a one-line notice
  (`legacy_config_notice`). Trust-record key unchanged (`the_trust_records_move_with_the_config_dir`).
- Endpoint keys: an inline `api_key` is `EndpointError::InlineApiKey` (exit 2, names `api_key_env`); the realm
  projection writes `CredentialSourceSpec::Env` only (a configured endpoint without a variable is refused); the 0600
  path and `Notice::EndpointPlainKey` are gone. Tests: `an_inline_api_key_is_a_typed_startup_error_naming_api_key_env`,
  `endpoints_are_projected_into_the_realm_document_once` (document holds the variable name only).
- Verification (lane `tui-port`): fmt clean; clippy `-p meerkat-tui --all-targets --all-features -D warnings` clean;
  nextest lib (`bootstrap::`, `endpoints::`, `cli::`) + `--test confinement` + `--test models`: 70 passed; confinement
  cases print `SKIPPED:` (AppArmor userns restriction on this host), DNS test included; `--help` exit 0; a real
  `rkat-tui --confinement=workspace` printed the AppArmor explanation and exited 2.

## Fork — the TUI's `fork` tool (2026-11-06)

- Goal: let the agent hand a side quest to a child that starts with its own conversation, behind the gate, while
  `fork_off` stays denied (`session::UNGATED_SPAWN_TOOLS`: its child seats through the engine's persisted-fork path
  without the TUI's spawn customizer, and it refuses a caller that is not a mob member).
- Probe, Route 1 (exact context: `PersistentSessionService::fork_durable_session` of the caller's session into a plain
  child): **dead**. A tool call runs inside the caller's turn; without a target the fork runs under
  `DurableForkSourceAdmission::Quiescent`, whose `quiescent_fork_mutation_guard` answers `SessionError::Busy` as soon as
  the source has an active runtime admission (at the committed prefix count too). The admission that forks a running
  caller, `CallerTurn`, exists only on a `DurableSessionForkTarget`, which carries a required `MobMemberBinding`: the
  child's metadata gets that binding and comms name (verified before commit) and its resume builds it as a member, so a
  plain child is not reachable through public API; `fork_durable_session_at_turn_boundary` waits for the turn to end
  (a deadlock from inside it). Not pursued further: the activated-instructions question
  (`ForkInstructionRefused`). Pinned by `tests/fork.rs::the_engine_refuses_a_durable_fork_of_a_running_session`
  (parent turn hanging in its model call; `fork_durable_session(id, None | Some(committed), None, None)` → `Busy`).
- Route 2 taken (text context): `fork::ForkToolDispatcher` (a TUI callback tool, provenance `rkat-tui.fork`, in the
  main session's gateway only; helpers get their tools from the helper host, so fork children cannot fork). It reads
  the caller's committed conversation with `load_committed_session_tolerating_tail` (the strict
  `load_authoritative_session` fails mid-turn with a transcript revision conflict against the running turn's tail), adds
  the running turn's request (not committed yet) from the approval engine's record of the user's newest words
  (`ApprovalEngine::latest_user_input`; `ToolDispatchContext::current_turn` is `None` for text turns), renders it
  (`fork::context`: user/assistant text, tool calls with truncated args, results clipped to 1 KiB, `/remember` system
  context, compaction summaries; the caller's system prompt left out; 48 KiB, newest kept; content cannot close the
  `<parent_conversation>` tag) and runs the child as a helper of the caller in its implicit mob through
  `DelegationExecutionService` (the path `delegate` takes) with member identity `fork-N`, the TUI's tool access policy
  and an override of the implicit `delegate` profile with builtins and shell on (that profile enables comms only; a
  `delegate` helper gets its parent's tools through `InheritParent`, which the host cannot resolve). Result: the
  child's bounded text (16 KiB) or an error result. (`MobMcpState::mob_spawn_helper` was tried first: from inside the
  caller's tool call the child never reached its first model call in the rig; not investigated further.)
- Everything else is the helper machinery: customizer gate for the parent as owner, `HELPER_PERSONA` + fork framing in
  the prompt (one task, report back, do not ask the user), file tools, curated shell env, follower (`[fork-1 · model]`,
  status bar, `/cost`), `Helpers::cancel_for` on interrupt.
- Policy: `ToolFamily::Fork` (provenance `rkat-tui.fork` only) and `fork` in `DELEGATION_TOOLS` → tier 1 like `delegate`;
  a `fork` from any other surface stays foreign.
- `mobs/follow.rs` follow failure: already emitted `HelperUpdate::Unmeasured` through `HelperRegistry::follow_failed`;
  unit test added (`a_helper_that_cannot_be_followed_is_reported_unmeasured`).
- Tests: `tests/fork.rs` — `a_fork_starts_with_the_parents_conversation_and_returns_its_answer`,
  `a_forks_calls_pass_the_parents_gate` (child `shell` denied by the classifier, counted on the parent, notice
  `[fork-1] shell`, `fork` itself never classified), `an_interrupt_leaves_no_running_fork`,
  `fork_off_stays_denied_and_a_fork_cannot_fork` (parent offers `fork`, not `fork_off`; child has `read_file`/`shell`,
  neither `fork` nor `fork_off`), the probe above; unit `fork_is_tiered_like_delegate_only_from_the_tuis_own_surface`,
  `fork::context` tests, the catalog family test.
- Upstream ask: a host hook to shape fork seats (the spawn customizer for persisted-fork children). It only matters once
  the main session is a durable mob member, which would make `fork_off` (exact context) usable behind the gate.

## Review fixes — 2026-11-06 (sealing, fork prompt, model switch)

- S1 hard links: `Shared::check_writable` refuses (Unix) a target with `nlink() > 1` (recoverable error naming the
  hard link); `commit_write` writes through `paths::write_atomic` with the new `FileMode::Preserve` (temp file renamed
  over the target, the old permission bits kept, a new file `0666` minus the umask), so a shared inode is never
  written. Tests: `tests/file_tools.rs` `write_and_edit_refuse_a_hard_linked_file` (`ln .git/config notes.txt`; both
  tools refuse, both names keep `[core]`), `writes_replace_the_file_and_keep_its_mode`; `paths` unit
  `atomic_write_never_writes_a_shared_inode`.
- S2 sealing outside the workspace: `ProjectRoot::with_sealed_roots` (canonicalized as far as they exist) and
  `paths::git_dirs` (the `.git` entry at the root and every ancestor: a directory, or a `gitdir:` file; plus the
  git dir's `commondir`; no subprocess). `bootstrap::scope::resolve_scope` seals the git dirs, the realm root and the
  user config dir. Tests: `paths` unit `sealed_roots_seal_state_outside_the_project`; `tests/approval_policy.rs`
  `sealed_roots_outside_the_workspace_are_sealed` (repo-subdir workspace, worktree `gitdir:` + `commondir`, realm,
  config dir; `write_file` and patch target Sealed, `read_file` not).
- A5 `.rkat` itself: `SEALED_PATTERNS`/`PROTECTED_PATTERNS` gain `.rkat` (`.rkat/**` does not match the base).
  Test: `sealed_paths_are_the_gate_state_and_vcs_internals_and_imply_protected` covers `.rkat`.
- S3 order-dependent patch check: `builtin_tier` checks every `FileWrite` target for a seal before the
  outside/protected loop. Tests: `policy` unit `a_later_sealed_patch_target_seals_the_patch`, `tests/approval_policy.rs`
  `a_sealed_patch_target_after_a_protected_or_outside_one_is_sealed` (`build.rs` before `.git/config`, `/tmp/x`
  before `.rkat/config.toml`, under `apply_patch *`).
- S4 forged roles in the fork prompt: `fork::context::entry` indents every continuation line (any line break,
  `\r`/VT/FF/NEL/U+2028/U+2029 included) with `  | `. Test: `content_lines_cannot_pose_as_entries`.
- S5 fork child tooling: `working_profile` takes `ParentTooling` (the caller session's persisted `SessionTooling`
  builtins/shell resolved against the TUI factory's defaults, both on) instead of forcing both on. Note: the realm
  `[tools] shell_enabled` is not read by the factory for TUI sessions (`build_factory` sets `.builtins(true)
  .shell(true)`), so the parent's effective tooling is its session overrides over those defaults. Test:
  `the_child_gets_the_parents_builtins_and_shell`.
- S6 proxies: README "Environment" says proxy URLs pass unchanged, credentials included.
- S7 + A6 state-dir error: `error::shell_quote` (`'\''` escaping) for the `mv`/`--state-root`/`--realm` hint, which
  is attached only when `error::suggests_move_aside` finds a schema/ledger/manifest incompatibility
  (`StoreError::{SchemaFromTheFuture, UnledgeredDomainObjects, ManifestFromTheFuture, CorruptLedger}` anywhere in
  the source chain). Tests: `a_state_dir_from_another_version_names_both_ways_out`,
  `other_open_failures_get_no_move_aside_advice` (governed realm, manifest lock timeout, permission error),
  `hint_paths_with_quotes_stay_one_shell_word`.
- C1 failed rebuild: `rebuild_session_workaround` returns `RebuildError` (`Retire`, `KeptPrevious` after a resume
  retried with the previous trigger, `Detached` when both resumes fail); `set_model` words the notice by kind
  (`model_switch_rebuild_notice`) and skips the trigger upkeep on a detached session. Tests (unit; the rig cannot
  inject a resume failure): `a_detached_session_is_reported_as_such_and_points_at_resume`,
  `a_session_still_running_only_keeps_its_helpers_on_the_old_model`.
- C2 repeated request: `render` skips the running turn's request only when the last conversational user message
  has its text and no assistant reply since ended a turn (each one asked for tools: the running turn's own
  committed tool rounds, which `the_conversation_renders_without_the_system_prompt` shows can be in the snapshot).
  Test: `a_repeated_request_after_an_answer_is_still_added`.
- C3 + A2 fork labels and errors: `fresh_label` takes the handle and lists `list_members_including_retiring`;
  `fork()` returns a `thiserror` `ForkError` rendered at the tool boundary; the handle is fetched once.
- C4 context entries: tool calls clipped to 1 KiB as a whole, assistant blocks in their own order. Test:
  `a_huge_tool_call_is_clipped_and_blocks_keep_their_order`.
- A1 delegate guards in `fork`: `admit_child_tool_policy()` before the implicit mob is ensured, and the handle must
  be the caller's implicit mob (`owner_bridge_session_lifecycle_authority`: implicit flag and bridge session ==
  caller). Test (unit): `only_the_callers_own_implicit_mob_is_acted_on`.
- A3 `fork::context` reuses `approval::transcript::truncate_text` (now `pub(crate)`).
- A4 helper policy merge: `deny_ungated_tools` merges the TUI denies into any policy (none/`Inherit` → the TUI
  list, deny list + names, allow list − names, constraints + a deny constraint, `ReadOnly` kept). Tests:
  `a_helper_without_a_policy_or_inheriting_gets_the_tui_deny_list`, `a_helpers_deny_list_gains_the_tui_names`,
  `a_helpers_allow_list_loses_the_tui_names`, `a_read_only_helper_stays_read_only_and_constraints_gain_a_deny`.
- C5 vacuous env test: `workspace_mode_passes_the_curated_environment` reruns itself with `RKAT_TUI_TEST_SECRET` set
  (unsafe `set_var` is denied) and asserts the confined shell does not see it.

## Dogfood follow-ups (2026-11-06, evening)

- `/model` picker vanished while a turn ran: an escalation prompt replaced the open picker and `advance_escalation` cleared the modal afterwards. Now a picker is parked while escalations show and restored once none remains; key repeats of Enter/Esc/Ctrl-C never act in modals (kitty reports repeats). Tests: `ui::tests::escalation_parks_the_model_picker_and_restores_it`, `parked_picker_returns_after_the_last_queued_escalation`, `stopping_all_escalations_restores_the_parked_picker`, `escalation_parks_the_resume_picker_and_restores_it`, `repeated_enter_and_esc_do_not_act_in_modals`.
- A prompt re-sent with the same text right after a failed turn was committed again (a failed run keeps its input; verified with a `RejectTools` probe): the runner now sends `(retrying the request above)` with a notice instead. Tests: `integrator::the_same_prompt_after_a_failed_turn_is_not_added_twice`, `a_different_prompt_after_a_failed_turn_is_sent_as_written`.
- `/compact now` notice reworded to say the compaction happens at the start of the next message; the read-before-edit refusal says a restart forgets earlier reads. Mouse copy: OSC 52 only (user's choice), written after the frame, clipboard + primary, honest notice.
- Classifier wording and visibility (a model's report from inside the TUI): (1) a block for lack of user intent said "the user never requested …" / "agent-authored exercise instructions" although the user had said "do the exercise in EXERCISE.md"; the classifier sees only the user's words, so stage 2 is now told (`prompts::STAGE2_INSTRUCTIONS`, `NOT_COVERED_REASON`) never to claim the user did not ask or that the request was agent-authored (only `<agent_written_files>` are) and to give "Not covered by what the user asked for in this session; ask the user to confirm before doing it." (2) The denial tag `[read 0 files, 2 searches]` read as the session's reads; it is now `[classifier checked 0 files, 2 searches]` (`1 file`, `1 search` singular). (3) A classifier allow was indistinguishable from a tier-1 call: `ClassifierVerdict::Allow` now carries its reason (stage 1: "nothing flagged by the quick check"; a cache hit: "allowed earlier in this session"), the `Decision` event carries it, and the runner sends `Msg::ToolNote` instead of the old separate approval line; the transcript shows it as one dim `allowed by classifier: <reason>` line (one line, ≤ 100 chars) under the tool block (`Block::Tool.note`, attached by tool-use id, a helper's call by `{scope}/{id}`, held until a late block arrives), hidden with a folded helper. Tests: `approval::prompts::tests::stage2_asks_for_confirmation_instead_of_blaming_the_agent`, `approval::classifier::tests::{run_suffixes, verdict_display}`, `runner::approval::tests::classifier_allow_note_is_one_short_line`, `ui::transcript::tests::{notes_attach_to_tool_blocks_by_call_id, a_helper_calls_note_hides_with_its_fold}`, `integrator::a_classifier_allowed_call_gets_a_note_and_a_tier1_call_does_not`, `classifier_llm::{stage2_reads_build_rs_then_decides, the_tool_loop_is_bounded_and_ends_with_a_toolless_call}`.
- Protected writes ask the user: the classifier allowed a `write_file .env.dogfood` once and denied the same write once (stage-2 variance on a secrets file). A `write_file` / `edit_file` / `apply_patch` with a protected target inside the project (`paths::PROTECTED_PATTERNS`, not sealed) is now `Tier::AskUser` ("ask the user"), between tier 2 and the classifier: deny rule → sealed → allow rule → ask → classifier; outside-only targets still go to the classifier, a patch mixing a protected and an outside target asks. The engine skips the classifier, auto mode and its pause and escalates with `EscalationPrompt.policy_reason` (`protected path: \`.env.local\` (secrets); the policy always asks for these`, shown as `Policy: …` in the dialog); the verdict is `AskUser · Human`, the answer leaves the denial counters alone, auto mode off still asks. "Allow + rule" mints the existing literal rule over the project-relative path (`write_file .env.local`). Tests: `approval::policy::tests::{tier_table (rows .env, .env.local, build.rs, AGENTS.md, .gitmodules, tui.toml, a patch with .cargo/config.toml), protected_writes_ask_the_user_unless_a_rule_or_seal_decides, protected_kinds_name_what_the_path_is}`, `approval_policy::protected_path_edit_asks_the_user` (was `protected_path_edit_goes_to_classifier`), `approval::engine::tests::{protected_writes_ask_the_user_and_never_the_classifier, auto_off_still_asks_for_protected_writes}`, `approval_service::a_protected_write_asks_the_user_and_allow_always_stops_the_prompts`, `integrator::a_protected_write_is_put_to_the_user_and_runs_when_allowed`.
- Known rare flake: `approval::engine::tests::parallel_escalations_share_one_timeout_including_the_queue` failed once while test binaries were compiling concurrently (heavy load), passed in 5×644 full runs and 6 lib runs afterwards. Hypothesis: with `start_paused`, the clock auto-advances past the escalation timeout while the ledger's `spawn_blocking` submit is still running on a starved thread. Not fixed; revisit if CI shows it.

- Helper panel and full view (user verdict on the inline fold: the header lines scroll away with the output; wanted Claude Code's behaviour). A panel under the input (`render::draw_panel`, at most `MAX_PANEL_LINES` = 6, `… and N more` past that, hidden when empty) lists `Transcript::panel_helpers`: running helpers and those finished during the current turn (`HelperFold.this_turn`, cleared by `retire_finished_helpers` when a turn starts), the newest top-level first with its nested helpers under it. Ctrl+↑/↓ move a panel selection (was: inline header focus); Enter on an empty input opens the helper's full view in place of the transcript (`render::draw_helper_view`, own `ScrollState`, follows at the bottom), Enter with text submits, Esc closes the view then drops the selection; Ctrl-O / Ctrl-T act on the view's blocks. Helper `TextDelta`/`TextComplete` and `ReasoningDelta`/`ReasoningComplete` now become `Block::Assistant` / `Block::Reasoning` owned by the helper's fold (per-helper open streams, separate from the session's), so they show in the view and in the inline unfolded header. Inline headers stay; a click still unfolds one. Tests: `ui::tests::{the_panel_lists_running_helpers_newest_first_and_drops_finished_ones_at_the_next_turn, a_nested_helper_is_listed_under_its_parent, the_panel_shows_at_most_six_lines, ctrl_down_selects_the_newest_helper_and_ctrl_up_down_wrap, enter_with_an_empty_input_opens_the_full_view_without_submitting, enter_with_text_submits_despite_a_selection, esc_closes_the_view_then_drops_the_selection, the_helper_view_follows_new_output_while_at_the_bottom, a_helpers_prose_and_reasoning_are_in_its_view_in_order, a_reset_drops_the_selection_and_the_view}`, `ui::transcript::tests::{helper_text_and_reasoning_become_blocks_owned_by_its_fold, finished_helpers_leave_the_panel_when_a_turn_starts}`, `render_snapshot::{the_helper_panel_renders_under_the_input, a_helper_full_view_renders_title_prose_reasoning_and_tools, an_expanded_helper_renders_its_blocks (was ..._and_the_focus_is_reversed), a_folded_helper_renders_one_line (rows shifted for the panel)}`.
- Input soft wrap (user request: long lines scrolled sideways): `input::wrap_layout(lines: &[String], width: usize) -> WrapLayout` wraps each logical line at the field width by display width (sanitized: tab 4, wide 2, zero-width 0 and kept on the previous char's row; char boundaries, no word wrap; an empty line one row; a line whose last row is exactly full gets an empty row where its end cursor sits). `WrapLayout::{rows, to_visual, from_visual}` map logical `(row, col)` to visual `(vrow, vcol)` and back (a boundary position belongs to the later row). The stored lines are unchanged; `render::draw_at` sets `InputEditor::set_wrap_width(width - 2)` each frame, the field height is the visual row count clamped to 1..=`MAX_INPUT_LINES` (6, the existing cap), and the field scrolls so the cursor's row is visible. Up/Down move by visual rows keeping the display column (clamped); history recall stays on the first/last visual row; Home/End stay logical. The transcript area (and `set_transcript_rows`, so mouse hit-testing) shrinks with the taller input. Tests: `ui::input::tests::{wrap_layout_splits_a_long_line_at_the_width, wrap_layout_never_splits_a_wide_char, wrap_layout_gives_an_empty_line_one_row, wrap_layout_maps_row_ends_to_the_next_row, wrap_layout_puts_the_cursor_after_an_exactly_full_line_on_the_next_row, wrap_width_zero_does_not_wrap, up_and_down_move_by_visual_rows}`, `ui::tests::{up_moves_a_visual_row_within_a_wrapped_line_keeping_the_column, down_moves_a_visual_row_within_a_wrapped_line_then_recalls_newer_history}`, `render_snapshot::a_long_input_line_wraps_and_the_transcript_shrinks`; no existing snapshot changed.

## Helper depth limit (2026-11-06)

- Dogfood: helper A delegated helper B, each carrying a full context (~150-180k tokens for five trivial calls). Helpers now nest at most `[helpers] max_depth` levels (`tui.toml`/`--set helpers.max_depth=N`, default 1, a project file may only lower it). A helper's depth is its `HelperTag.path` length; at the limit the customizer denies `delegate`, `mob_create`, `mob_spawn_member`, `council` in its tool access policy (`NESTING_TOOLS`, merged by `deny_ungated_tools`), turns the member's mob tool category off so the schemas leave its tool list (~25k fixed tokens per call), and `Depth::admit` refuses a fresh spawn beyond the limit (a restored member keeps running, tools denied). The helper persona says a helper denied delegation does the work itself.
- Tests: `mobs::customizer::tests::{a_helper_at_the_depth_limit_is_denied_the_nesting_tools, a_helper_below_the_depth_limit_keeps_the_nesting_tools, a_spawn_beyond_the_depth_limit_is_refused}`, `approval::rules::tests::helper_depth_defaults_to_one_layers_and_the_project_only_lowers_it`, `mobs::{a_helper_at_the_default_depth_cannot_delegate, a_helper_below_a_raised_depth_limit_delegates_once_more}`.

## Typing during a slow tool (2026-11-06)

- Dogfood report: typing while `shell sleep 10` ran "killed the run" (`InterruptedByRecovery`, the call
  `outcome_uncertain`, the typed text not delivered). The realm's input rows show otherwise: both steers ("Test",
  "Hepp") and the prompt ended with the TUI's own interrupt reason (`runtime_terminated: "rkat-tui: interrupted by
  the user"`, history `queued → abandoned (TrackedCancel)`), 3.7 s after the second steer; the second run was
  interrupted 1 s after its shell call started with no steer at all. The only producer of that reason is
  `SessionHandle::interrupt` (`src/session/mod.rs`, `INTERRUPT_REASON`), reached from Esc / Ctrl-C /
  Ctrl-X Ctrl-S / escalation Stop. The recovery notice is the engine closing a *cancelled* run's provisional tail
  (`meerkat-runtime/src/runtime_loop.rs` `settle_cancelled_run_tail` → `recovery.rs` `close_interrupted_run`), not
  a steer side effect. The engine's Steer waits for the tool: pinned by
  `integrator::a_message_typed_while_a_slow_tool_runs_reaches_the_model_after_its_result` (real shell `sleep 2`, the
  steer lands after the tool result in the same run, no system notice).
- Likely trigger (UI, not changed here): Esc with only *admitted* steers is not a recall but an interrupt
  (README "Esc limitation"), and Ctrl-C interrupts even right after a mouse selection. Suggested: Esc/Ctrl-C with
  admitted steers or an active selection should not interrupt on the first press.
- Real runner bugs found while reproducing (`integrator::interrupting_a_slow_tool_returns_the_messages_typed_meanwhile`,
  failed about 1 run in 2 under parallel load before the fix):
  - a steer admitted before the interrupt but not yet bound to the run escaped: `stop_run` leaves it queued, the
    runtime stages it into a follow-up run, and the following `cancel_input_if_present` failed with
    `InterruptDispatchOutcomeUnknown { reason: "executor reported the exact run non-current while machine authority
    still binds it" }`; the follow-up turn then ran after the user's interrupt;
  - that error aborted `SessionHandle::interrupt` (`?`), dropping the inputs it had already cancelled, so the turn read
    `failed: rkat-tui: interrupted by the user` and only one message came back to the editor.
  Fix: `interrupt` first abandons the outstanding inputs the runtime still holds `Queued`, then stops the run and
  cancels the rest; a cancel that reports `InterruptDispatchOutcomeUnknown { run_id }` stops that run; up to three
  passes retry what failed; it returns `Interrupted { cancelled, errors }` and never drops what it cancelled (the runner
  shows leftover errors as `interrupt incomplete: …`). 12 parallel runs of the integrator suite clean afterwards.
- Engine ask: an atomic "stop this run and abandon these queued inputs" (or a `stop_run` option that abandons the
  steer lane), and `cancel_input_if_present` abandoning an input staged into a run the executor has not entered
  instead of `InterruptDispatchOutcomeUnknown` (`meerkat-runtime/src/meerkat_machine/runtime_control.rs`
  `cancel_input_if_present_owned_before`, `user_interrupt.rs:164`). The TUI's queued-first ordering narrows but
  does not close the window between its state read and the cancel.
- UI follow-up (same day): Esc while a turn runs no longer interrupts while anything can be pulled back.
  `SessionHandle::recall_queued(&[InputId]) -> Recalled { recalled, errors }` abandons each outstanding input the
  runtime still holds `Queued` (`cancel_input_if_present`, reason `rkat-tui: recalled by the user`) and never touches
  the run; `Effect::RecallQueued` runs it in the runner loop (awaited, so the recalled steers' completions find them
  marked: no turn result, not `undelivered`, removed from the classifier transcript) and answers `Msg::Recalled`. The
  UI sends the recall when an admitted (or unacknowledged) steer is pending, returns the recalled and held messages to
  the editor oldest first, and marks the admitted ones left as reached ("your messages already reached the agent; Esc
  again interrupts"); only then, or with nothing pending, does Esc interrupt. The first Ctrl-C with a mouse selection
  only clears it. Tests: `integrator::{recalling_a_message_typed_during_a_slow_tool_leaves_the_run_alone,
  recalling_a_steer_that_already_joined_the_run_leaves_it_delivered}` (5 repeated runs clean),
  `ui::tests::{esc_with_an_admitted_message_recalls_it_without_interrupting,
  esc_when_the_messages_already_reached_the_agent_notices_then_interrupts,
  esc_recalls_only_held_messages_and_otherwise_interrupts, esc_and_other_keys_clear_the_selection}`. Remaining
  window (same as the interrupt's): a steer staged into the run between the `Queued` read and the cancel meets a
  cancel of a staged input, which the runtime may answer by interrupting that run or with an error (shown as
  "could not recall: …"); the engine ask above (abandon-if-still-queued as one operation) closes it.
- Backspace deleted two characters in kitty 0.32.2: with `DISAMBIGUATE_ESCAPE_CODES | REPORT_EVENT_TYPES` kitty sent one tap as two `\x7f` bytes (captured with `printf '\e[>3u'; kitty +kitten show_key -m unchanged`), the release of a legacy-encoded key emitted as a second press, which the protocol says must not be sent. The TUI now requests disambiguation only (`src/ui/runtime.rs`); the modal key-repeat guard stays but sees only presses. Kitty's changelog 0.33–0.50 names no fix.

## Batch 1 A1 (2026-11-06)

Editor and key rebinds to match Claude Code (`.rct/rkat-tui/ux-parity.md` §2). Every step: `repo-cargo clippy -p
meerkat-tui --all-targets --all-features -- -D warnings` exit 0, `repo-cargo nextest run -p meerkat-tui
--all-features` exit 0 (lane `tui-port`).
- Ctrl+Enter = send now (`Action::SendNow` → `interrupt_and_send`, as Ctrl-X Ctrl-S); `\`+Enter at a line end
  is a newline (`InputEditor::backslash_newline`). Running/recall hints name Ctrl+Enter (render snapshot assertions
  updated). Tests: `keymap::tests::enter_variants`, `ui::tests::{ctrl_enter_sends_now, newline_variants_and_paste}`,
  `input::tests::a_trailing_backslash_becomes_a_newline`.
- Ctrl+O toggles the last reasoning block and the last tool output together (`Transcript::toggle_last_detail_in`:
  both expand when either is collapsed); Ctrl+T is unbound. Tests: `ui::tests::{ctrl_o_toggles_the_last_reasoning_and_tool_output_together,
  click_toggles_tool_output_and_ctrl_o_toggles_the_last}`, `keymap::tests::control_keys`.
- Kill ring in `InputEditor` (16 entries; consecutive kills join one entry, forward kills append, backward ones
  prepend): Ctrl+K (new; at a line end kills the newline), Ctrl+U, Ctrl+W / Ctrl/Alt+Backspace (at a line start
  kills the newline instead of only moving), Alt+D (new). Ctrl+Y yanks, Alt+Y cycles right after a yank. `/copy [N]`
  replaces Ctrl+Y copy (same `copy_text` OSC 52 path); the Ctrl+Y exemption from clearing a mouse selection is gone.
  Tests: `input::tests::{kills_feed_the_ring_and_yank_puts_the_newest_back, consecutive_kills_join_one_entry,
  alt_y_cycles_older_entries_only_right_after_a_yank, the_kill_ring_keeps_the_newest_entries}`,
  `ui::tests::{kill_and_yank_keys_edit_the_input, copy_command_copies_the_nth_latest_answer}`,
  `commands::tests::simple_commands`, `keymap::tests::control_keys`.
- Ctrl+P/Ctrl+N map to the editor's Up/Down action (rows, then history; a held message on an empty input).
  Tests: `ui::tests::ctrl_p_and_ctrl_n_are_up_and_down`, `keymap::tests::control_keys`.
- Esc Esc (second within `DOUBLE_ESC_WINDOW` = 1.5 s) on an idle draft with nothing to recall clears it and saves it
  to the prompt history (in memory and, through `take_submitted`, the history files); the first Esc arms it and
  the editor title says "Esc again to clear the input". Esc Esc on an empty input does nothing. Esc while running is
  unchanged. Tests: `ui::tests::esc_esc_clears_the_draft_into_history`,
  `render_snapshot::esc_on_an_idle_draft_names_the_second_esc_in_the_editor_title`.
- Ctrl+_ undo (also Ctrl+7 and Ctrl+Shift+-): `InputEditor` keeps up to `UNDO_DEPTH` = 64 snapshots (text and
  cursor), pushed at a typing word boundary, at the start of a run of deletes, and before kills, yanks, pastes,
  newlines, history recalls and clears (Ctrl+C's clear included); sending a prompt empties the stack. Tests:
  `input::tests::{undo_steps_back_by_words_and_edits, the_undo_stack_keeps_the_newest_states}`,
  `ui::tests::ctrl_underscore_undoes_input_edits`, `keymap::tests::control_keys`.
- Ctrl+S stash: a bare Ctrl+S (`Action::Stash`; the Ctrl+X chord is resolved before the keymap, so Ctrl+X Ctrl+S
  still sends now) stashes the input's text and cursor and clears it, or restores the stash on an empty input; a
  stash replaced by a newer one goes to the prompt history. The editor title names Ctrl-S while a stash waits on an
  empty input. Tests: `input::tests::stash_puts_the_prompt_aside_and_brings_it_back`,
  `ui::tests::{ctrl_s_alone_stashes_and_restores, chord_expires_after_window, chord_ctrl_x_ctrl_s_interrupt_and_send_when_running}`,
  `render_snapshot::a_stashed_prompt_is_named_in_the_editor_title`.
- Ctrl+L: `Effect::Redraw`, kept by the UI loop (`runtime::apply` sets a flag, never forwards it; the runner's arm
  is a no-op) and answered with `terminal.clear()` before the next frame. Tests:
  `ui::tests::ctrl_l_requests_a_redraw`, `runtime::tests::ctrl_l_asks_the_loop_for_a_full_repaint_and_nothing_else`,
  `keymap::tests::control_keys`.
- Shift+Tab (`KeyCode::BackTab` → `Action::CycleMode`) runs `/auto on` when the status says `off`, else `/auto off`
  (a paused mode counts as on), through the same `Effect::SetAutoMode` and runner notices. Tests:
  `ui::tests::shift_tab_switches_auto_mode_off_and_on`, `keymap::tests::control_keys`.
- Ctrl+D on an idle empty input arms `QUIT_CONFIRM_WINDOW` (3 s) and the editor title says "Ctrl-D again to quit";
  the second press quits. Busy behaviour (warning notice, interrupt, save undelivered) and `/quit` are unchanged;
  with text Ctrl+D still deletes forward. Tests: `ui::tests::{ctrl_d_quits_only_on_empty_input,
  quit_while_busy_needs_confirmation_and_hands_over_held_messages, idle_quit_is_immediate}`,
  `render_snapshot::a_first_idle_ctrl_d_asks_for_a_second_in_the_editor_title`.

## Batch 1 A2 (2026-11-06)

Runner and command changes to match Claude Code (`.rct/rkat-tui/ux-parity.md` §1–2). Every step:
`repo-cargo clippy -p meerkat-tui --all-targets --all-features -- -D warnings` exit 0, `repo-cargo nextest run -p
meerkat-tui --all-features` exit 0 (lane `tui-port`).
- Esc while running always interrupts (`Effect::Interrupt`, as Ctrl+C); after an interrupted turn the undelivered
  steers start the next turn through `send_unsent` (shown in place as user blocks); a failed/unknown/pending turn
  still returns them to the editor with the notice; Ctrl+D quit still saves them. The Esc recall path moved to ↑
  on an empty input (`take_back_queued`): held messages come back locally, admitted ones through the runner's
  `Effect::RecallQueued` (kept for ↑). Old Esc unit tests were rewritten deliberately. Tests:
  `ui::tests::{esc_interrupts_even_with_queued_messages, up_with_an_admitted_message_recalls_it_without_interrupting,
  up_when_the_messages_already_reached_the_agent_notices_then_recalls_history, up_recalls_the_newest_held_message,
  interrupted_turn_sends_undelivered_messages_next, interrupted_turn_without_queued_messages_starts_nothing,
  discarded_delivery_of_a_stopped_turn_is_sent_again, failed_turn_returns_undelivered_with_a_notice}`,
  `integrator::esc_interrupts_and_the_queued_message_runs_next` (real `App` driving the runner with the scripted
  client), `render_snapshot::queued_block_status_count_and_hints`.
- `/compact` (bare, or `now`) → `CompactCommand::Now` → the existing `Effect::CompactNow` rebuild; `/compact status`
  prints the policy (`CompactionPolicy::describe`); `/compact <instructions>` compacts the same way and adds the warn
  notice `COMPACT_FOCUS_UNSUPPORTED` (the instructions are dropped). User-facing texts say `/compact` instead of
  `/compact now`. Engine ask: **instruction-guided compaction** (a way to pass focus text to the compaction summary
  call, e.g. through the `CompactionCurator` seam or a per-trigger instruction), so `/compact <instructions>` can
  work as in Claude Code. Tests: `commands::tests::remember_and_compact_commands`,
  `ui::tests::{bare_compact_compacts_and_instructions_add_a_warning,
  compact_status_reports_the_policy_and_compact_needs_context, compact_now_refused_while_running_or_pending}`,
  `compaction::tests::describe_names_the_threshold_and_the_commands`,
  `compact::warn_mode_warns_and_compacts_only_on_compact` (now types a bare `/compact` into a real `App`).
- Command names: `/permissions` primary with `/rules` alias (same `parse_rules`; `RULES_USAGE` and the realm-config
  notice name `/permissions`), `/usage` primary with `/cost` and `/stats` (usage error `/usage`), `/tasks` an alias of
  `/helpers`. Tests: `commands::tests::{permissions_and_rules_parse_identically, tasks_is_helpers,
  remember_and_compact_commands, help_mentions_every_command_and_key}`, `approval::rules` notice test.
- `--resume` is `Option<Option<String>>` (`num_args = 0..=1`); a bare `--resume` is `Start::Pick`: a fresh session
  and `Effect::RequestResumePicker` as the first effect (sent by `runner::run` and the test harness before the
  prompt), the same path `/resume` alone takes. `--resume <ref>` and `-c` unchanged (`-c` still conflicts). Tests:
  `lifecycle::tests::start_follows_the_flags`, `cli::tests` (parse), `integrator::resume_without_a_reference_opens_the_picker_at_start`.
- `/context` (`/context all` too): `App::context_header` (gauge: used/window, percent, estimated flags; then the
  `/compact status` policy text) → `Effect::ContextReport` → `Runner::context_report` adds, without new engine
  calls, the session's system-message bytes and message counts (`SessionHandle::read`), the instruction files on
  disk (SYSTEM.md, workspace AGENTS.md, `InstructionSources::discover` sections, with sizes or "absent"), and the
  count of tools rkat-tui registers (the external gateway); the engine's built-in/MCP/helper tools are labelled
  unknown. Tests: `ui::tests::context_command_sends_the_gauge_and_the_policy`,
  `runner::context::tests::the_context_breakdown_counts_the_prompt_messages_files_and_tools`,
  `commands::tests::remember_and_compact_commands`, `integrator::context_report_describes_the_session`.
- `/status` → `Effect::StatusReport` → `Runner::status_report` (`runner/status.rs`): one notice from `StatusReport`'s
  `Display` with `CARGO_PKG_VERSION`, the session identity's model/provider and its endpoint, the full session id,
  workspace, realm dir and id, `--confinement` mode, the auto state (`auto_status`, as the status bar), the host
  trust record's verdict on the realm `tui.toml` and the project `.rkat/mcp.toml`, and `[helpers] max_depth`
  (`Runtime::helper_max_depth`, new; "not built in" without helpers). Tests:
  `runner::status::tests::the_status_report_names_everything_on_its_own_line`,
  `ui::tests::slash_commands_map_to_effects`, `commands::tests::remember_and_compact_commands`,
  `integrator::status_report_names_the_session_and_the_workspace`.
- `/export [file]`: `Transcript::export_text` (a `## ` header per shown block; folded reasoning, collapsed tool
  output and folded helpers' blocks keep only headers) → `Effect::Export { path, text }` → `Runner::export`
  (`runner/export.rs`): `paths::write_atomic` with `FileMode::Private`, the workspace as trusted root for the default
  `<workspace>/.rkat/tui/export-<session>-<unix secs>.txt`, the parent of a typed path (relative to the workspace,
  `~/` expanded) otherwise. Like the history and cost files it is the TUI's own write: no tool policy or seal.
  Tests: `transcript::tests::export_text_has_a_header_per_block_and_folds_collapsed_output`,
  `runner::export::tests::{the_default_export_path_is_under_the_workspace_realm_dir,
  an_export_round_trips_through_the_atomic_write}`, `ui::tests::slash_commands_map_to_effects`,
  `integrator::export_writes_the_transcript_and_names_the_file`.
- `/init` (idle only; refused with the busy notice while a turn runs or messages are queued) starts a turn with
  `commands::INIT_PROMPT` through `start_turn`, so the prompt shows as a user block; the agent writes AGENTS.md with
  its file tools under the usual approval/protected/sealed-path rules. Tests:
  `ui::tests::init_sends_the_canned_prompt_as_a_user_turn_when_idle`, `commands::tests::remember_and_compact_commands`.
- Bell: `EnableFocusChange` in TUI mode (disabled on restore); `Event::FocusGained/FocusLost` → `Msg::Focus`; the App
  (focused at start) sets a due bell on `TurnFinished` when no queued message started the next turn and on
  `Msg::Escalation`, only while unfocused and `[ui] bell` is on (`UiSettings::bell`, default true, layered like
  `helpers`); the loop writes BEL after the next frame (`take_bell` → `ring_bell`). Tests:
  `ui::tests::the_bell_rings_only_while_unfocused_and_enabled`, `runtime::tests::{translate_filters_release_events,
  the_bell_is_one_bel_byte}`, `rules::tests::{ui_bell_defaults_to_on_and_layers, rules_file_round_trip}`. Not run
  in a real terminal (no interactive terminal in this session).
- Alt+P (`Action::ModelPicker`) runs `/model`'s picker path (`Command::Model(ModelCommand::Pick)`, same idle check)
  without touching the editor: the picker modal takes the keys and the input survives Esc and a selection. Tests:
  `ui::tests::alt_p_opens_the_model_picker_and_keeps_the_draft`, `keymap::tests::control_keys`.
- Ctrl+Z (`Action::Suspend` → `Effect::Suspend`, kept by the UI loop like `Redraw`): `outside_tui` drops the
  crossterm `EventStream` (its reader thread stops), restores the terminal, runs `stop_self`
  (`nix::sys::signal::raise(SIGTSTP)`: safe, no process launch; `libc::raise` would need `unsafe`, which the
  workspace denies), and on `fg` re-enters TUI mode, drains pending input and clears for a full repaint. Off Unix it
  is a no-op. `nix` (workspace dep, already in the lock) is a `cfg(unix)` dependency of meerkat-tui now; Cargo.lock
  gains the edge, `crates/meerkat-tui/BUILD.bazel` was regenerated (`node scripts/generate-bazel-rust-builds.mjs`,
  which also picked up the integrator test's `:package_runfiles` data from earlier commits: the commit hooks are not
  installed in this checkout), and `make buildbuddy-lock-update` refreshed MODULE.bazel.lock
  (`scripts/check_bazel_module_lock_inputs.py .`: 66 recorded inputs match; `generate-bazel-rust-builds.mjs --check`
  exit 0). Tests: `keymap::tests::control_keys`, `ui::tests::ctrl_z_requests_a_suspend`,
  `runtime::tests::ctrl_z_suspends_in_the_loop_and_nothing_reaches_the_integrator`. The stop/continue itself was
  not run in a real terminal.
- `/memory` (item 8) and Ctrl+G / Ctrl+X Ctrl+E (item 11) waited for an engine exemption; they landed in
  "Batch 1 A2b" below.

## Batch 1 A2b (2026-11-06)

- Engine: `meerkat_core::HostLaunchExemption::UserEditor` (`user_editor`, in `ALL`, reason and display name) with
  the constructor `HostLaunchExemption::user_editor(program, untrusted)` (built by `launch` like
  `credential_command`; the host resolves the program). `crates/meerkat/tests/host_launch_sites.rs` maps it to its
  constructor and lists its one site, `crates/meerkat-tui/src/ui/editor.rs`. The launch table in
  `docs/concepts/configuration.mdx` and the platform skill's `references/security.md` name it. The TUI passes no
  untrusted-path judgement (`None`): the editor runs with rkat-tui's own environment, as the user started it.
- External-editor primitive: `Effect::ExternalEditor(EditorTarget)` stays in the UI loop (like `Redraw`/`Suspend`).
  `run_external_editor` resolves the editor (`$VISUAL`, else `$EDITOR`, else `vi`; a set, non-empty variable wins;
  split on whitespace, so `code --wait` is program `code` and argument `--wait`; a blank-only variable is refused
  with a notice without leaving TUI mode), then `outside_tui` (now async and shared with Ctrl+Z) drops the event
  stream, restores the terminal (raw mode, alternate screen, mouse capture, bracketed paste, focus reports, keyboard
  flags), runs the editor on a blocking thread with inherited stdio, re-enters TUI mode, drains the keys typed
  meanwhile and repaints; `Msg::EditorClosed` carries the outcome back.
- Ctrl+G and Ctrl+X Ctrl+E (`Action::ExternalEditor`, chord arm in `main_key`; bare Ctrl+E still goes to the line
  end): the draft is written to a private temp file in `<realm>/drafts/` (removed afterwards), and on success the
  file's text minus one trailing newline replaces the input (one undo step, so Ctrl+_ restores it); a non-zero exit
  or start failure keeps the draft and notices. Idle and running. The chord hint now reads
  `C-x- · C-s send now · C-e edit in $EDITOR`.
- `/memory [user|system]`: `Effect::OpenMemory` → the runner resolves the workspace `AGENTS.md` (the `/remember`
  target), the user `AGENTS.md` or `SYSTEM.md` (none without a user config dir: notice), creates the directory and a
  missing `SYSTEM.md` with `SYSTEM_MD_HEADER` (a header-only file counts as empty in `apply_system_md`), records the
  bytes before and answers `Msg::EditFile`. After a successful run, `Effect::MemoryEdited`: unchanged → notice; a
  changed `AGENTS.md` goes into the live session through `append_system_context` (as `/remember`) under
  `# Instructions (updated from <path>)`, capped at `AGENTS_MD_MAX_BYTES` like a loaded file, keyed by a SHA-256 of
  that text; `SYSTEM.md` or an emptied file applies from the next session. Allowed idle and running (the reload path
  works while a turn runs, as `/remember` does).
- Tests: `editor::tests::{the_editor_is_visual_then_editor_then_vi, a_blank_editor_variable_is_refused,
  a_draft_round_trips_through_its_temporary_file, a_failed_editor_reports_and_keeps_nothing,
  a_real_editor_command_edits_the_draft}` (the last runs `sh -c` through the exemption), `runtime::tests::
  {the_editor_runs_in_the_loop_and_its_answer_reaches_the_app, a_failed_editor_run_keeps_its_target}`,
  `ui::tests::{ctrl_g_and_ctrl_x_ctrl_e_edit_the_draft_externally,
  the_edited_draft_replaces_the_input_and_a_failure_keeps_it,
  memory_opens_the_file_the_integrator_names_and_reports_back}`, `context::tests::
  {memory_opens_the_workspace_user_or_system_file, memory_keys_follow_the_context}`, `instructions::tests::
  {a_system_md_with_only_its_header_counts_as_empty, an_edited_file_becomes_capped_system_context}`,
  `commands::tests` (parse, help), `keymap::tests::control_keys`, `render_snapshot::
  a_pending_ctrl_x_chord_names_its_second_keys`.
- Commands: `clippy -p meerkat-core --all-targets --all-features -D warnings` exit 0; `nextest run -p meerkat-core
  confinement` 41 passed; `nextest run -p meerkat --test host_launch_sites` 12 passed; `clippy -p meerkat-tui
  --all-targets --all-features -D warnings` exit 0; `nextest run -p meerkat-tui --all-features` 760 passed, exit 0 (lane
  `tui-port`).
- Commit split: the exemption's commit carries rkat-tui's `ui/editor.rs` too, because
  `every_host_launch_exemption_is_in_use_and_named_where_it_launches` fails an exemption without a site; that tree
  was checked on its own (meerkat-tui clippy exit 0, the editor tests pass). `node
  scripts/generate-bazel-rust-builds.mjs` added `//crates/meerkat-tui:cargo_manifest` to the `host_launch_sites`
  test's data (it now names a meerkat-tui source); `validate-mintlify-docs.py` ok.
- Manual check: not run in a real terminal (no interactive terminal in this session, and starting rkat-tui needs a
  live endpoint). The launch itself is exercised by `a_real_editor_command_edits_the_draft` (`sh -c` through
  `HostLaunchExemption::user_editor`); the leave/restore path is the one Ctrl+Z uses, also not run interactively.

## Review defects (2026-11-07)

Two defects from the Meerkat maintainer's review of the harness, one commit each.

### 1. Shell lexer: a carriage return is not a word separator

- Before: `lex_raw` flushed a word on `' ' | '\t' | '\r'`, and a `#` at a word start opened a comment, so
  `git status \r# ; rm -rf x` lexed as `git status` and passed the Tier-1 allowlist while the shell ran `rm -rf x`.
- Observed shell behaviour (`printf '<cmd>\n' | <shell> | od -c`), bash 5.2.21, dash 0.5.12 (`/usr/bin/sh`), zsh 5.9
  (`zsh -f`, from the Ubuntu noble `zsh_5.9-6ubuntu2` package extracted locally; zsh is not installed on this host):
  - `echo a\r#b c` prints `a\r#b c` in all three: `\r` is a word character, the `#` after it is not a comment.
  - `echo x \r# ; echo INJECTED` prints `x \r#` and then `INJECTED` in all three (also through `bash -c`/`zsh -fc`).
  - `echo crlf\r\necho next\r\n` prints `crlf\r` and `next\r`: `\r\n` is not a line ending, the word keeps the `\r`.
  - `\x0b`, `\x0c`, `\x1b` are word characters in all three; NUL is dropped by bash and dash and kept by zsh.
  - `echo a#b "#" \#c` → `a#b # #c`; `echo a=#b ''#x` → `a=#b #x`; `echo a;#b` and `echo a\t#b ; echo INJ` → `a`;
    `echo a >#x` is a syntax error in all three (the `#` after `>` starts a comment).
- Choice: any control character other than `\t` and `\n` (`char::is_control`: C0, DEL, C1) makes the command
  `Unsupported("control character")`, so Tier 1 never allows it and allow rules never match it (`allow_matches`
  refuses unlexable commands); the gate escalates. Modelling `\r` as a word character instead would be correct but
  is not needed: no allowlisted command contains one, and a `\r\n` command is not the command it looks like (the
  shell runs `status\r`), so judging it with the `\r` stripped would judge a different command. A `\r\n` line ending
  therefore escalates too.
- Audit of the rest of the lexer and `attach_redirect_targets` against the same shells: the only blanks are space
  and tab and the only line separator `\n` (the metacharacters `| & ; < > ( )` are operators or refused); `#` opens
  a comment only when no word is in progress (mid-word `a#b`, `a=#b`, quoted `"#"`/`'#'`, escaped `\#`, and `''#x`
  after an empty quoted word stay words; after `;`, a blank, or a redirection operator it is a comment, the last
  leaving the redirect without a target, which is refused); `$#` is refused as parameter expansion, quoted or not.
  No other disagreement found.
- Tests (`src/approval/shell.rs`): `control_characters_are_unsupported` (the exact `\r#` case, `\r` inside a word and
  inside quotes, escaped `\r`, `\r\n` endings, NUL, ESC, `\x0b`, `\x0c`, DEL, U+0085; tab and newline still split),
  `carriage_return_comment_is_not_allowlisted`, `hash_starts_a_comment_only_at_a_word_start`.
- Commands: `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo nextest run --all-features`
  763 passed, 6 skipped, exit 0.

### 2. Trust boundary for every executable project input

- Before: only the project `.rkat/mcp.toml` had a trust decision (`/mcp trust`, `--allow-project-mcp`). The workspace
  realm `.rkat/tui/config.toml` was loaded by `scope::load_config` and read by the factory through
  `RealmConfigStore` on every build with no trust check, so its `[[tools.mcp_servers]]` entered the MCP set and its
  `[hooks]` became the build's active hooks; the project `.rkat/config.toml`'s `[hooks]` were discovered by the
  factory at the conventions root (`meerkat::sdk::resolve_layered_hooks`: build config > `<context_root>/.rkat/
  config.toml` > `<user_root>/.rkat/config.toml`, by id; that is the only project hook file the engine reads). All
  run as the host, and on Linux a confined shell can write `.rkat/`.
- After (`src/bootstrap/project_trust.rs`, `ProjectInputs`): the inputs are `mcp.toml`, the project
  `.rkat/config.toml`, and the realm `config.toml` when it lies inside the workspace (a `--realm`/`--state-root` realm
  elsewhere is the user's own). Each is trusted when absent, when its whole-file SHA-256 is in the existing
  `HostTrustRecord` (`~/.rkat/tui/trust/<key>.toml`, the same record `/rules trust` uses), or for one run under
  `--trust-project` (alias `--allow-project-mcp`). Untrusted:
  - `mcp::load` leaves the realm config's servers out and the trusting loader refuses `mcp.toml`'s; both are listed
    as `Refused`.
  - `RealmConfigStore` runs `ProjectInputs::guard` on every config it serves (so on every session build, helpers'
    included, and re-checking the record each time): an untrusted realm config loses its `tools.mcp_servers` and
    `hooks.entries`; each hook of an untrusted project `.rkat/config.toml` gets a disabled entry of its id in the
    build config, which the factory's id layering puts first, so the project entry never runs (an entry the user's
    `~/.rkat/config.toml` states identically is the user's and is kept). User-global hooks are untouched.
  - One start-up notice names each untrusted file with the reason (`Untrusted::NotRecorded` "may have come with the
    repository", or `Changed` "it changed since rkat-tui last wrote or you trusted it") and what it did not start,
    and says `/trust` and `--trust-project`.
- `/trust` (new; `/mcp trust` stays as an alias) records every present input and reloads the MCP set. rkat-tui's own
  writes into the realm config (endpoint projection, model profiles, `ConfigStore` patches) record the new digest
  only when the file was trusted (or absent) before the write, so a write never launders planted content. `/status`
  lists each input's state.
- Tests: `project_trust::tests::{the_inputs_are_the_project_files_and_the_workspace_realm_config,
  a_realm_config_from_the_checkout_runs_nothing_until_trusted, project_hooks_are_shadowed_until_trusted,
  a_hook_the_user_file_states_too_is_the_users, rkat_tui_writes_keep_trust_but_never_launder_a_planted_file}`
  (initial checkout refused with notice, trust admits across a reopen, modification after trust refused again with
  "changed", the flag trusts without recording; hook shadowing checked against the engine's own
  `meerkat::resolve_layered_hooks_config`), `mcp::tests::{project_and_user_files_compose,
  a_realm_config_server_waits_for_trust}` (a server defined only in the realm config is not in the MCP set before
  `/trust`, is after, and is refused again after a change), `commands::tests` (`/trust`, `/mcp trust`),
  `status::tests::the_status_report_names_everything_on_its_own_line`.
- Commands: `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo fmt --all -- --check` exit 0;
  `cargo nextest run --all-features` 769 passed, 6 skipped, exit 0.
- Residual (closed): the guard reads the project `.rkat/config.toml` when it serves the build config and the factory
  reads it again while building, so a hook a concurrently running confined command added between the two reads was
  not shadowed for that one build. The engine now asks the host about the bytes it read: zond/meerkat `414265f683`
  ("fix: the factory asks the host about the hook source bytes it read", branch `rkat-tui-pin`) adds
  `meerkat::HookSourceTrust` and `AgentFactory::with_hook_source_trust`; the factory reads each convention hook file
  once, hashes the exact bytes it parsed (`HookSourceDigest`, SHA-256) and calls `decide(HookSource { path, sha256 })`;
  every hook of an `Untrusted` source keeps its slot and is refused at each invocation with
  `ConfinementRefusal::UntrustedConfigurationSource` (an entry a trusted user file states identically stays the
  user's). `ProjectInputs` implements it (`HostTrustRecord::holds_digest`: the project `.rkat/config.toml` is trusted
  when the record holds the digest of those bytes, or under `--trust-project`; any other path, the user file
  included, is the user's) and bootstrap installs it on the one factory every build uses. The guard's disabled-entry
  shadowing stays: a refused guardrail withholds what it gates (a `pre_tool_execution` one blocks the tool) while a
  shadowed one is disabled, so a file known untrusted before the build stays inert as the notice says, and only
  content changed after the guard's read fails closed. The guard still owns the start-up notice and the realm
  config's MCP servers and hooks, which the engine predicate does not cover. Tests:
  `project_trust::tests::{the_hook_source_trust_judges_the_bytes_the_factory_read,
  the_hook_source_trust_keeps_a_workspace_that_is_the_user_root}`; `tests/hook_source_trust.rs`
  (`a_hook_written_between_the_guard_and_the_build_never_runs` writes the hook between `RealmConfigStore::get` and
  `AgentFactory::build_agent`, the two steps `FactoryAgentBuilder` takes, with a control without the trust that runs
  it; `a_hook_the_user_file_states_identically_survives_the_refusal`;
  `a_project_file_untrusted_before_the_build_stays_inert`); the engine's reproducer is
  `crates/meerkat/tests/hook_source_trust.rs`. The realm config's other sections (endpoints, models) still load
  untrusted; they start no process, but a planted endpoint `base_url` could receive an environment key. Not in
  this change's scope.

## Review reuse items (2026-11-07)

Three "reuse the existing owner" items from the maintainer's review ("Existing owners to reuse before adding APIs").

### 1. Realm lease while the realm is open

- Before: bootstrap opened the realm persistence (`meerkat::open_realm_persistence_in`) but took no lease, so
  `rkat realm prune` and realm inspection (`meerkat_store::inspect_realm_leases_in`) saw a realm a TUI had open as
  idle.
- After: `bootstrap_with` calls `meerkat_store::start_realm_lease_in(state_root, realm_id, None, "rkat-tui")` right
  after persistence opens, as `rkat-rpc` (`crates/meerkat-rpc/src/main.rs`), `rkat-rest` and the mob host
  (`crates/meerkat-cli/src/mob_host.rs`) do. The guard lives in `Runtime::realm_lease` for the process lifetime;
  the runner's quit (`Runner::shutdown`) releases it with `RealmLeaseGuard::shutdown` (heartbeat stopped, record
  removed) after the service shuts down; a runtime dropped without quitting releases it through the guard's `Drop`.
  A failure to write the record is the typed startup error `TuiError::RealmLease` (realm id, lease directory,
  `StoreError`).
- What the owner API provides: a per-instance heartbeat record (`<realm>/leases/<instance>.json`, refreshed every
  `REALM_LEASE_HEARTBEAT_SECS`, stale after the TTL) that `inspect_realm_leases_in` classifies as active/stale/
  unparseable. What it does not: exclusivity. The lease is not a lock; every host writes its own record and no host
  refuses to start while another holds one (with `instance_id: None` each process gets a fresh `instance-<uuid>`).
  So there is no "held by another host" condition to surface; a second `rkat-tui` on the same realm starts and holds
  a second lease, exactly like a second `rkat-rpc`.
- Tests (`tests/integrator.rs`): `each_open_of_a_realm_holds_its_own_lease_until_released` (two bootstraps of the same
  workspace realm: two active leases, surface `rkat-tui`, this pid; releasing one leaves one; releasing both leaves
  none and `blocks_destructive_prune()` is false; release is idempotent), `quitting_releases_the_realm_lease` (the
  runner's quit removes the record).
- Commands: `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo nextest run --all-features`
  771 passed, 6 skipped, exit 0.

### 2. Sessions listed through `SessionService::list`

- Before: `src/session/resolve.rs` listed `SessionStore::list(SessionFilter::default())` on the realm store
  (`Runtime::store`) for `last`/`~N`/prefix resolution and the `/resume` picker.
- After: `resolve_session_ref` and `list_sessions` take `&dyn SessionService` and call
  `SessionService::list(SessionQuery::default())` (the listing `rkat`'s `resolve_flexible_session_id` reads),
  returning `SessionSummary`. `Runtime::store` is gone (it had no other reader). Semantics kept: `last`/`~`/`~0`,
  `~N` (0-based), full UUID, unique id prefix, ordering by `updated_at` then id, newest first (sorted here: the
  service does not promise an order). The picker reads `session_id`, `updated_at` and `message_count`, all in
  `SessionSummary`; nothing the resolver needs is missing.
- Behaviour change from the owner: `PersistentSessionService::list` reads the runtime session catalog and leaves out
  sessions whose lifecycle is `Archived` or whose runtime state is `Retired`/`Destroyed`; the raw store listing
  included them. A session the TUI closed on switch or quit stays listed (the `-c`/`/resume` tests below cover it).
- Tests: unchanged `tests/integrator.rs` `new_and_resume_switch_sessions_in_process`,
  `continue_and_resume_by_id_across_lifetimes`, `resume_without_a_reference_opens_the_picker_at_start` pass (the
  harness's `Rig::store` is now the service as `Arc<dyn SessionService>`); new
  `references_resolve_over_the_service_listing` (two sessions listed newest first, `~1` and a prefix resolve to the
  older; after archiving it through `archive_with_machine_protocol` it is neither listed nor `~1` nor a prefix
  match). The harness also exposes `Rig::machine` for that archive.
- Commands: `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo nextest run --all-features`
  772 passed, 6 skipped, exit 0.

### 3. Realm config mutations through `ConfigRuntime`

- Write sites found (every mutation of the realm `config.toml`; nothing else rkat-tui writes is a `Config`):
  1. `bootstrap_with` → `endpoints::project_endpoints`: the `[realm.rkat-tui-endpoints]` backend/auth/binding
     entries of configured endpoints.
  2. `bootstrap_with` → `endpoints::ensure_model_profile`: the start-up model's `[models.<id>]` profile.
  3. `Runtime::ensure_profile` → `ensure_model_profile`: a profile on a model switch (picker, `/model`, helpers).
  4. `RealmConfigStore`'s own `ConfigStore::{set, patch, patch_with_warnings}`: called only by the engine's
     `ConfigRuntime` over the builder's config store (the LLM reconfigure host built by
     `build_runtime_backed_service_with_default_reconfigure_host`, same `config_state.json`), which already holds
     the writer lock.
  Not config writes: the trust records (`HostTrustRecord`, `~/.rkat/tui/trust/<key>.toml`, its own file), `/remember`
  (appends to `AGENTS.md`), `/permissions add` (`tui.toml` rules), the approval ledger.
- Before: sites 1-3 read `FileConfigStore::get` and wrote `FileConfigStore::patch` on the document directly, with
  `RealmConfigStore::{before_write, after_write}` bracketing the trust record outside any lock; no generation was
  read or published, so a concurrent `rkat config set` could interleave and a TUI write never bumped the generation
  other surfaces' CAS reads.
- After: `RealmConfigStore` owns a `meerkat_core::ConfigRuntime` over the raw document (`RealmDocument`, a private
  `ConfigStore` whose writes bracket the trust record) at `RuntimeScope::config_state_path()`
  (`<realm>/config_state.json`, the sidecar the engine's reconfigure host and `rkat config` use; one source for both
  now). Sites 1-3 go through `endpoints::patch_document(runtime, delta_for)`: `ConfigRuntime::get` (reader lock,
  document + generation), build the patch from the snapshot, `ConfigRuntime::patch(delta, Some(generation))`
  (writer lock, generation check, publication); on `GenerationConflict` re-read and rebuild, up to 8 attempts
  (`EndpointError::ConfigContended` after). This is the read-modify-write `rkat` follows (`cli_head_config_snapshot`
  + `patch_cli_skills`). The trust bracket now runs inside the writer lock, and records only after a successful
  write. `before_write`/`after_write`/`document()` and `RealmConfigWrite::UNGUARDED` are gone. Site 4 keeps writing
  the document directly (it is the lock holder's store; routing it through rkat-tui's runtime would wait on the lock
  its caller holds).
- Reads: `scope::load_config` (the start-up config) now reads through `ConfigRuntime::get` too, as `rkat config get`
  does. The factory's per-build reads (`RealmConfigStore::get`) stay lock-free `ConfigStore` reads, as the engine's
  own `FactoryAgentBuilder` reads its config store.
- What the owner API provides: the cross-process writer/reader lock (`config_state.lock`, OS advisory), the
  generation sidecar and its CAS. What it does not: a retrying read-modify-write helper (each surface writes its own
  loop; `patch_document` is rkat-tui's), and a path in its errors (`EndpointError::ConfigWrite` now carries the
  `ConfigRuntimeError` without the document path).
- Tests: `endpoints::tests::a_write_that_lost_the_generation_race_is_rebuilt_and_retried` (another writer commits
  between the read and the write: the patch is rebuilt from the new document, both entries kept, generation 7→8),
  `a_write_waits_for_the_writer_lock` (a held `ConfigWriterLock` keeps the profile write pending; released, it
  lands), `endpoints_are_projected_into_the_realm_document_once` (now through the runtime; two writes, generation 2;
  the unchanged projection writes nothing), `profiles_are_written_for_unknown_models_and_refused_across_dialects`,
  `realm_config::tests::reads_follow_the_document_and_writes_reach_it` (a runtime write publishes generation 1; a
  stale-generation write is `GenerationConflict`), `project_trust::tests::
  writes_through_the_config_runtime_keep_trust_but_never_launder` (absent → written → trusted; trusted → written →
  trusted; planted → written → still `Changed`).
- Commands: `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo fmt --all -- --check` exit 0;
  `cargo nextest run --all-features` 775 passed, 6 skipped, exit 0.

## Batch 1 A3 (2026-11-07)

### 1. Approval prompt: numbered rows with a cursor

- Before: one key line `[y] allow once  [a] allow + rule  [n] deny  [s] stop`; only the letters answered, Esc denied.
- After: `ui/modal.rs` `EscalationDialog` (the cursor; `Modal::Escalation` carries it) renders three numbered rows,
  `1. Yes, allow once`, `2. Yes, allow and add a rule (<tool> <subject>)` (the literal rule the runner would add:
  `EscalationPrompt::rule`, set in `runner/approval.rs` `on_escalation` from the first subject; without one the row
  reads `Yes, allow (no rule fits this action)`), `3. No, and tell the agent what to do instead (Esc)`, then the
  footer `Enter select · s stop the turn · Ctrl+C stop all`. ↑/↓ and `j`/`k` move (clamped; a held key moves but
  never answers), Enter picks the highlighted row, `1`-`3` pick directly; `y`, `a`, `n`/Esc, `s` and Ctrl+C keep
  their meaning. A Ctrl/Alt chord no longer answers (Ctrl+Y used to allow once). Every new or advanced dialog starts
  on row 1.
- Tests: `ui::modal::tests::escalation_dialog_moves_clamped_and_picks`,
  `ui::tests::escalation_choices_by_number_arrows_and_enter` (four queued escalations answered by `2`, ↓↓↑ Enter,
  `1` and `s`; a held `j` moves; Ctrl+Y does nothing), `render_snapshot::escalation_modal_is_drawn_over_the_transcript`
  (full dialog snapshot, the cyan cursor row, ↓ moves the cursor to row 2). The parked-picker and queued-escalation
  tests pass unchanged.
- Docs: HELP_TEXT "Approval prompt" (one line per choice), README "How approval works", CHANGELOG.
- Commands: `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo fmt --all` exit 0;
  `cargo nextest run --all-features` first run 771 passed, 1 failed (`workspace_state_survives_a_rename`, the known
  timing flake), rerun 777 passed, 6 skipped, exit 0.

### 2. Deny with a comment

- Before: `n`/Esc denied at once; the model read `the user denied this action` and nothing else.
- After: picking row 3 (`3`, `n`, Esc, Enter on it) opens `EscalationDialog::comment` (one line; control characters
  dropped, newlines and tabs become spaces, a paste goes in, 2000 chars max; letters are text, `s` included; a held
  key types or deletes but never answers). Enter sends `EscalationAnswer::Deny { comment }` (trimmed; `None` when
  empty), Esc sends `Deny { comment: None }`, Ctrl+C still stops every prompt. The comment travels as
  `HumanDecision::Deny { comment }` to the engine, which records `denied: <comment>` as the approval ledger's decision
  reason and denies with `DenyReason::Human { comment }`: its display is `denied by the user: <comment>`, and
  `deny_message` then says `Do what the user says instead; do not try to achieve the denied action's effect another
  way.` instead of the "choose a safer approach or ask" tail. Without a comment the wording is unchanged. The broker's
  fail-closed answers are `Deny { comment: None }`. The approval transcript records no decisions, so it is unchanged.
- Tests: `ui::modal::tests::escalation_dialog_moves_clamped_and_picks` (every way into the field, Esc and blank Enter
  deny without a comment), `the_comment_field_takes_text_and_denies_with_it` (typing, Backspace, paste, repeat, cap),
  `ui::tests::deny_with_a_comment_then_the_queue_and_the_parked_picker` (three queued escalations over a parked model
  picker: a typed + pasted comment, Enter on row 3 then empty Enter, Ctrl+C in an open field; the picker returns),
  `escalation_parks_the_model_picker_and_restores_it` / `escalation_parks_the_resume_picker_and_restores_it` (updated:
  Esc/`n` open the field, a second key denies), `approval::admission::tests::denials_are_policy_denied_with_the_reason`,
  `render_snapshot::escalation_deny_comment_field_snapshot`, and end to end
  `approval_service::a_deny_comment_reaches_the_model_as_the_tool_error` (a protected `.env.local` write denied with
  a comment: the tool error the model receives contains `denied by the user: put it in .env.example instead`, the
  file is not written, the ledger record is Denied with reason `denied: put it in .env.example instead`).
- Commands: `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo fmt --all` exit 0;
  `cargo nextest run --all-features` 781 passed, 6 skipped, exit 0.

### 3. `?` shortcut panel

- Before: `?` was always typed; the keys were only in `/help`'s notice.
- After: `?` (no Ctrl/Alt) on an empty input, outside reverse search and the Ctrl-X chord, opens `Modal::Keys`, drawn
  by `draw_modal` as a box up to 104 columns wide with `commands::keys_help()` (HELP_TEXT from `Keys:` through the
  approval prompt, the heading dropped since the title says `Keys (any key closes)`). A description too long for its
  column wraps under it; a panel taller than the screen ends in `… more in /help`. Any key press closes it and is not
  typed (Enter does not send); a held `?` does not close it. With text in the input `?` is typed. An escalation
  closes the panel instead of parking it; a resume or model picker list replaces it. The editor title reads `? keys`
  when idle with an empty input and nothing else to say.
- Tests: `ui::tests::question_mark_on_an_empty_input_toggles_the_keys_panel`,
  `the_keys_panel_gives_way_to_escalations_and_pickers`,
  `ui::commands::tests::the_keys_panel_lists_the_keys_and_the_approval_prompt_only`,
  `render_snapshot::keys_panel_snapshot` (the idle `? keys` title; the panel at 110×10, cut with `… more in /help`; the
  wrapped layout at 60 columns; a key closes it), `renders_blocks_input_and_status_bar` (border row now carries the
  hint).
- Commands: `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo fmt --all` exit 0;
  `cargo nextest run --all-features` 785 passed, 6 skipped, exit 0.

### 4. Docs pass for batch 1

- Read: every commit from `f65a305` (Ctrl+Enter, the first batch 1 commit) to HEAD, including the five review commits
  (`2f0c267` shell lexer, `ba9d4cd` one trust decision, `d9871dc` realm lease, `a39bc5c` `SessionService::list`,
  `d33c0ff` `ConfigRuntime` writes), against `ui/keymap.rs`, `ui/commands.rs` `parse_command`, `cli.rs`,
  `runner/status.rs` and the `draw_input` hints.
- HELP_TEXT: `/quit` names the lease release; `/resume` says archived sessions are not offered; `/trust` names
  `--trust-project`; `/permissions rm` names its `remove` alias; `/status` lists the trust of each file `/trust`
  covers (was "tui.toml and mcp.toml"); Ctrl/Alt+←/→ and the Ctrl-7 / Ctrl-Shift-- forms of Ctrl-_ are named.
- README: the command list names every alias once (`/?`, `/exit`, `/q`, `/new`, `/rules`, `remove`, `/cost`,
  `/stats`, `/mcp trust`, `/tasks`); `/status` describes the per-input trust lines; the stale "Ctrl-T is unbound"
  clause is gone; a "Claude Code parity" paragraph opens "Keys and commands" and points at `docs/ux-parity.md`.
- CHANGELOG: the `rkat-tui` paragraph pointed at `crates/meerkat-tui/README.md` (now `README.md`), named
  `/mcp trust`/`--allow-project-mcp` as the way to trust project MCP (now `/trust`/`--trust-project`), and still
  described Esc as pulling queued steers back and an interrupt as returning queued messages to the editor (both
  replaced in batch 1: Esc interrupts, ↑ pulls back, queued messages are sent next); the Ctrl+T and "Ctrl+Y no
  longer copies" wording is rephrased around the new controls; `/status`'s trust wording follows the code.
- Checked absent: no doc or hint names Ctrl+T, a Ctrl+Y copy, or Esc recalling queued messages
  (`grep -n "Ctrl+T\|Ctrl-T\|Ctrl+Y no\|Esc recalls" README.md CHANGELOG.md src/ui/commands.rs src/ui/render.rs`:
  no match).

## Batch 1 summary

Controls changed by batch 1 (before → after):

- Ctrl+Enter: newline → send now (interrupt and send; as Ctrl-X Ctrl-S). `\` before Enter at a line end: sent →
  newline.
- Ctrl+O: last reasoning block only → reasoning and last tool output together. Ctrl+T: last tool output → unbound.
- Ctrl+Y: copy → yank (kill ring; Alt+Y cycles). Ctrl+K: unbound → kill to line end. Alt+D: unbound → kill word
  after. `/copy [N]`: none → copy the Nth latest answer.
- Ctrl+P/Ctrl+N: unbound → ↑/↓.
- Esc Esc on an idle draft: nothing → clear it into the prompt history.
- Ctrl+_: unbound → undo input edits.
- Ctrl+S: unbound → stash / restore the input.
- Ctrl+L: unbound → clear and redraw the screen.
- Shift+Tab: unbound → auto mode off/on.
- Ctrl+D on an idle empty input: quit at once → quit on a second press within 3 s.
- Esc while a turn runs: pull back queued steers (interrupt only when none) → interrupt; queued messages are sent
  next. ↑ on an empty input takes them back.
- `/compact`: show the policy → compact now (`/compact status` shows the policy; `/compact <instructions>` warns).
- `/permissions` (was only `/rules`, now its alias), `/usage` (was only `/cost`; `/cost`, `/stats` aliases),
  `/tasks` (alias of `/helpers`).
- `rkat-tui --resume` with no reference: error → session picker.
- New commands: `/context`, `/status`, `/export [file]`, `/init`, `/memory [user|system]`.
- Terminal bell: none → on a turn end or an approval prompt while unfocused (`[ui] bell`).
- Alt+P: unbound → model picker, draft kept. Ctrl+Z: unbound → suspend (Unix). Ctrl+G / Ctrl+X Ctrl+E: unbound → edit
  the input in `$VISUAL`/`$EDITOR`/`vi`.
- Approval prompt: `[y] [a] [n] [s]` key line → numbered rows with a cursor (↑/↓, `j`/`k`, Enter, `1`-`3`; the
  letters still answer; `s` and Ctrl+C as a footer hint).
- Approval deny: immediate → row 3 opens a comment field; the comment reaches the model's tool error and the
  approval record.
- `?` on an empty input: typed → shortcut panel (any key closes it).
- `/mcp trust` → `/trust` (alias kept) over every project input; `--allow-project-mcp` → `--trust-project` (alias
  kept).

Commands for the docs pass: `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo fmt --all`
exit 0; `cargo nextest run --all-features` 785 passed, 6 skipped, exit 0 (`help_mentions_every_command_and_key`
covers the HELP_TEXT edits).

## Batch 2 A (2026-11-07)

### 1. `/` command menu with Tab completion

- Before: commands were parsed by a hand-written `match` in `ui/commands.rs` `parse_command`, and `/help`'s
  "Commands:" section was a separate hand-written string (`HELP_TEXT`); typing `/` offered nothing, Tab was unbound.
- After: `ui/commands.rs` `COMMANDS` is one `CommandSpec { name, aliases, usage, summary, help, parse }` table in
  `/help` order (19 commands, 27 names; `/mcp` has no help rows of its own, `/trust`'s row names it). `parse` looks
  the first word up with `spec(name)` and calls the spec's parser; `help_text()` (built once) generates the
  "Commands:" rows from the specs' `help` rows (left column padded to 33, continuation lines indented), then the
  hand-written `KEYS_HELP` and the `//` footer; `keys_help()` is `KEYS_HELP`. The generated text was checked
  byte-for-byte against the old `HELP_TEXT` before the Keys section gained its new row.
  `ui/complete.rs`: `word_at` finds the first word of a one-line input that starts with `/` (not `//`) while the
  cursor is in it; `candidates` lists `commands::completions(prefix)` (name or alias prefix, case-insensitive; an
  alias offers itself when the name does not match, e.g. `/ex` → `/exit`, `/export`) as `/name usage — summary`;
  `Menu` holds the rows, the highlight (wrapping) and an 8-row window that follows it. `App` keeps `menu` and
  `menu_dismissed`, refreshed after every message (the same word keeps the menu and its highlight). With the menu
  open, plain ↑/↓ move the highlight (not history), Tab replaces the word with the highlighted command plus a space
  (stepping over a space already there, so `/mo| gpt-5.5` → `/model gpt-5.5`), Enter does the same unless the word
  already names a command exactly (then it sends as before, arguments included), Esc closes the menu for that word
  (a changed word reopens it). Ctrl-_ undoes a completion (`InputEditor::replace_in_line`). `render.rs` `draw_menu`
  draws the rows over the bottom of the transcript right above the input (cleared first; highlighted row reversed,
  the others cyan label and dim summary, cut to the width), only without a modal; the editor title reads
  `Tab completes · ↑/↓ choose · Esc closes the menu` while it is open.
- Tests: `ui::commands::tests::every_spec_parses_and_every_name_has_a_spec` (every name and alias parses to a
  command and looks up its own spec, no name twice, the 27 names the old parser knew all have a spec, every spec is
  in `/help`, the help rows appear in spec order), `completions_match_names_and_aliases_by_prefix`;
  `ui::complete::tests::{the_command_word_is_the_first_word_of_a_one_line_slash_input,
  candidates_show_usage_and_summary, the_highlight_wraps_and_the_window_follows_it}`;
  `ui::tests::the_slash_menu_filters_moves_completes_and_closes` (filtering, ↓/↑ with a history entry present, Tab,
  undo, Enter completing `/he`, Enter sending `/status`, an alias, Tab before arguments) and
  `esc_closes_the_slash_menu_until_the_word_changes` (Esc, an agent event, retyping, Enter sending `/cop` as an
  unknown command, no menu for `/zz`, `//he`, `hi /he`, two lines, `/help x`);
  `render_snapshot::the_slash_menu_renders_above_the_input_with_the_highlight` (full 70×12 snapshot of `/co` with
  ↓: four rows over the transcript, the second reversed edge to edge, cyan labels, the title hint; Esc restores
  the transcript rows and the title). The helper panel, keys panel and title-hint snapshots pass unchanged.
- Docs: HELP_TEXT Keys gains the `/ (one-line input)` row; README "Keys and commands"; CHANGELOG.
- Commands: `cargo fmt --all` exit 0; `cargo clippy --all-targets --all-features -- -D warnings` exit 0;
  `cargo nextest run --all-features` 793 passed, 6 skipped, exit 0.

### 2. `@` file mentions with path completion

- Before: `@` was plain text; the agent had to read a file the user named itself.
- After: `src/mentions.rs` (new module). `workspace_paths(root)` walks the workspace with
  `tools::glob::mention_walker` (the `glob` tool's walker settings: `.gitignore`/excludes honoured, hidden files
  included, `.git` skipped, secrets not hidden; it also never enters a path matching `paths::SEALED_PATTERNS`,
  exposed as `paths::is_sealed_relative`, nor one of the root's sealed roots), in parallel, at most 100 000 paths,
  directories with `/`, sorted shallow first. `match_paths` returns up to 8: path prefix first, then file-name
  substring (case-insensitive), leaving out the directory the fragment already names. `ui/complete.rs` gains
  `CompletionKind::Path`: `word_at` finds a word that starts with `@` and ends at the cursor (any line; the char
  after the cursor must be blank or the line end; `a@b` is not one); its rows are the matches, a directory row
  `keep_open`. `App::with_workspace` (the runner passes `scope.project_root`) enables it; the index is built when a
  path word appears and dropped when it goes, so each menu opening rebuilds it. Tab/Enter insert the highlighted
  path: a file with a space (menu closes), a directory without one (the menu lists its contents); Enter on a fully
  typed file name sends.
  On send, `mentions::scan_mentions` finds the `@` words that resolve (through `ProjectRoot::resolve`, so `..` and
  symlinks cannot leave the workspace) to an existing file, directory or sealed path, once each (trailing prose
  punctuation is dropped when the word with it names nothing). `App::note_mentions` puts a dim
  `+ N file(s) (paths)` notice under the user block (also for a Ctrl-X Ctrl-S send-next prompt) and a warning
  `@<path> is sealed: not attached` per sealed one; the user block and the queued/held texts stay as typed. The
  runner expands where the text goes to the session (`Runner::start_turn` before the retry check, so a retried
  prompt compares the expanded text the session holds, and `Runner::steer`) with `mentions::expand_mentions`:
  after the prompt, per file `<file path="rel">` + content + `</file>` (UTF-8 only; at most 64 KiB, cut on a char
  boundary with a `[truncated]` line; 256 KiB for all blocks together), a binary/non-UTF-8 file or one past the
  budget as `<file path="…" note="not attached: …" />`, per directory `<directory path="rel/">` with its entries
  one per line (gitignore honoured, sealed skipped, at most 200, then `[… and N more]`). Sealed paths are never
  opened.
- Index time (`mentions::tests::index_build_time`, ignored, run with
  `cargo nextest run --all-features --run-ignored all --no-capture -E 'test(index_build_time)'`): this repository,
  139 paths in 3.8–5.4 ms; a synthetic 20 200-path tree (200 directories × 100 files) 105 ms with the serial walk,
  63 ms with the parallel walk, in the debug test build; a match over it 0.8 ms. Removing the sealed-path filter
  changed nothing (65 ms), so the walk itself dominates. The same parallel walk in a standalone release build of
  the `ignore` crate on the same tree shape: 10–13 ms over three runs. So the ~50 ms budget holds in release; the
  debug build is about 25% over it.
- Tests: `mentions::tests::{the_index_honours_gitignore_and_skips_sealed_paths (a gitignored target/, .git,
  .rkat), matching_is_path_prefix_then_name_substring, scanning_finds_words_that_start_with_at_and_name_something
  (punctuation, a@b, @nobody, a sealed path, a repeat, ../outside), expansion_attaches_files_listings_and_notes
  (a file, a directory, a binary file, a sealed path, a 128 KiB two-byte-char file cut to 64 KiB),
  the_total_budget_turns_later_files_into_notes, a_long_directory_listing_is_capped}`;
  `ui::complete::tests::{a_path_word_starts_with_at_and_ends_at_the_cursor,
  path_candidates_keep_the_menu_open_for_directories}`;
  `ui::tests::the_at_menu_completes_paths_and_stays_open_for_directories` (Tab into a directory, Enter on a file,
  substring, Enter sending a typed file, no sealed rows, no menu for `a@READ`) and
  `a_prompt_with_mentions_notes_what_it_attaches_and_what_is_sealed` (the StartTurn text as typed, the
  `+ 2 files (src/main.rs, src/ui/)` and sealed notices, singular, none for `a@b`);
  `render_snapshot::the_at_menu_renders_matching_paths_above_the_input`; end to end
  `integrator::an_at_mention_attaches_the_file_the_session_receives` (the scripted model's request and the stored
  session both hold the prompt plus the `<file path="notes.txt">` block).
- Docs: HELP_TEXT Keys gains the `@ (ending a word)` row; README "Keys and commands"; CHANGELOG; the module list in
  `lib.rs`.
- Commands: `cargo fmt --all` exit 0; `cargo clippy --all-targets --all-features -- -D warnings` exit 0;
  `cargo nextest run --all-features` 805 passed, 7 skipped (the ignored timing test is the new skip), exit 0.

## Batch 2 B (2026-11-07)

### `!` shell mode

- Before: an input starting with `!` was sent to the model as text (parity row 24, candidate 3 in
  `docs/ux-parity.md`).
- Engine finding: the agent's shell runs only as the `shell` tool (`meerkat_tools::builtin::shell::ShellTool`,
  dispatched behind the approval gate). Its confined launch path (`custody_spawn::spawn_in_custody`, the
  `ConfinementBinding` it compiles) is `pub(super)`, so the engine offers no host-callable confined runner. A user
  command is therefore launched through `meerkat_sandbox` directly (new `src/user_shell.rs`): `UserShell::new`
  takes the bootstrap's `ConfinementPlan::shell` and, for `ShellConfinement::Required`, compiles that exact
  requirement once with `CompiledConfinement::compile`; each run binds one `ProcessLaunchSpec` (program: the tool's
  resolved shell, `bootstrap::approval::resolved_shell_program`; args `-c <cmd>`; directory: the canonical
  workspace; environment: `PWD` plus `ConfinementPlan::shell_env`, the curated set, as the tool's
  `custody_spawn::environment` builds it) and spawns it with null stdin. A refused requirement refuses the run;
  nothing runs unconfined in a confined session. Under `ShellConfinement::TrustedHost` (`--no-confinement`) it runs
  with `tokio::process::Command` in its own process group, inheriting the environment, like the tool's
  trusted-host branch. No custody records: the run is not a tool call and lives only as long as its task.
- After: `ui::commands::parse_shell` (first non-blank character `!`, not `!!`; one optional space dropped; later
  lines belong to the command) and `unescape_bang` (`\!x` sends `!x`), checked in `App::submit` beside
  `parse_remember`, not as a `CommandSpec`. Idle: a `Block::Shell` (`$ <cmd>`, running) and `Effect::RunShell`.
  The runner (`runner/shell.rs`) runs it off the loop in a task, appends
  `[shell run by the user] $ <cmd>\n<output>\n(exit N)` (`user_shell::context_text`) with
  `SessionHandle::append_system_context` (the `/remember` call) under a per-run key
  (`rkat-tui:shell:<pid>-<nanos>-<counter>`), and only then answers `Msg::ShellFinished`, so a message sent after
  the command finds the context. The block shows `✓/✗ exit N (ms)` and the output preview (tool-result cap),
  foldable by click and Ctrl-O like tool output, and is exported by `/export`. Output: stdout and stderr read
  concurrently into one buffer in arrival order, capped at 64 KiB with `[output truncated at 64 KiB]` (reading
  continues past the cap so the command never blocks). Timeout: `[shell] user_timeout_secs` (new `ShellSection` in
  `tui.toml`, default 120, at least 1; a project file may only lower it; `--set shell.user_timeout_secs=N`); on a
  timeout, a cancel (Esc or Ctrl-C while one runs: `Effect::CancelShell` aborts the task) or the command's exit, its
  processes are killed (the process group of a plain run; on Linux the confined child's PID namespace dies with
  it). While a turn runs, or a command runs or waits, a `!` input becomes a `PendingState::Shell` item with a
  `$ <cmd>` placeholder labelled "runs after this turn"; messages typed while a command runs are held
  (`Deferred`). When the turn ends, `turn_finished` keeps what would start the next turn (unsent messages, the
  Ctrl-X Ctrl-S prompt) held and runs the oldest command in place of its placeholder; each `ShellFinished` runs the
  next, and after the last the held messages go as one turn through `next_after_turn` (a failed turn still returns
  its messages to the editor). ↑ takes a held command back as typed (`!cmd`). `/clear` and `/resume` wait for a
  running command. The editor title in shell mode: " shell mode: Enter runs it in the sandbox · output joins the
  context " (unconfined: "runs it (unconfined)"; busy: "runs it after the current work").
- Tests: `ui::commands::tests::bang_is_shell_mode_and_escapes` (`!`, `! `, `!  ls`, leading blanks, multi-line,
  `!!`, `!! wow`, `\!`, a mid-line `!`); `ui::tests::{bang_runs_a_command_and_escapes_send_text (RunShell, Esc →
  CancelShell, the done block, a late answer ignored, a bare `!`, `\!important` → `!important`, `!! wow` as text),
  shell_mode_shows_in_the_editor_title, commands_typed_during_a_turn_run_after_it_before_the_queued_messages (two
  commands and a steered message during a turn: the turn ends with the message undelivered → only the first
  command runs; a message typed meanwhile is held; the second runs; then one turn `two\n\nfour`),
  up_takes_a_held_command_back}`; `ui::render::tests::a_shell_block_shows_the_command_its_exit_and_its_output`;
  `user_shell::tests::{the_context_names_the_command_its_output_and_its_exit, output_is_capped_with_a_marker,
  a_plain_run_merges_both_streams_and_reports_the_exit, a_run_past_the_timeout_is_killed (a backgrounded `sleep`
  holding the pipes does not stall it), long_output_is_capped}`;
  `approval::rules::tests::the_user_shell_timeout_layers_and_the_project_only_lowers_it`;
  `runner::shell::tests::run_keys_are_unique`; end to end
  `integrator::a_bang_command_runs_and_joins_the_context_without_a_turn` (`echo hi; pwd` through the runner on
  the test rigs' `--no-confinement`: output `hi` and the canonical workspace, no model call, then the next turn's
  system prompt holds `[shell run by the user] $ echo hi; pwd\nhi\n` and `(exit 0)`) and
  `confinement::a_bang_command_runs_under_the_shells_confinement` (gated like the other confined cases by the
  direct sandbox probe, printing `SKIPPED:` where the backend is unavailable; on this host it ran: the write inside
  the workspace landed, the one outside did not, `ANTHROPIC_API_KEY` was unset inside).
- Docs: HELP_TEXT Keys gains the `! (first character)` row; README "Shell mode" paragraph and the
  `shell.user_timeout_secs` row of the configuration reference; CHANGELOG; the module list in `lib.rs`.
- Commands: `cargo fmt --all` exit 0; `cargo clippy --all-targets --all-features -- -D warnings` exit 0;
  `cargo nextest run --all-features` 820 passed, 7 skipped, exit 0.

## Batch 3 A (2026-11-07)

Permission modes on Shift+Tab (`docs/ux-parity.md` rows 9, 27, 28; candidate 5).

- Before: Shift+Tab toggled auto mode off/on. `/auto off` (and `--no-auto`) left tier 2 (in-project edits)
  allowed and denied every tier-3 action outright (`DenyReason::AutoDisabled`, no dialog); there was no
  accept-edits or plan mode. The on/off state was not saved at runtime (only `[auto] enabled` in a `tui.toml`
  the user edited).
- After: `approval::mode::PermissionMode` (`Manual` = Claude Code's `default`, `AcceptEdits`, `Plan`, `Auto`,
  the default) lives in the engine beside auto mode's counters (`AutoMode::mode`; `Paused{UserDisabled}` exactly
  when the mode is not auto). `ApprovalEngine::route` maps a tier decision through the mode: deny rules and
  sealed paths deny and tier 1 allows in every mode; a protected write asks in every mode but plan; manual asks
  the human for tier 2 and tier 3 (`Asking::Mode`, no classifier, counters untouched); accept-edits allows tier 2
  and asks for tier 3; auto is unchanged (classifier, pause, escalation); plan denies writes (file tools, patches,
  notes, protected ones included), shell commands unless tier 1 by the policy itself (not by an allow rule) and
  everything else above tier 1, with `DenyReason::PlanMode` (`PLAN_MODE_DENIAL`: "plan mode is on: … present your
  plan …"), which replaces the now unused `AutoDisabled`. An escalation queued behind another is routed again
  under the current mode when it gets the lock. Helpers inherit the mode (one engine).
  Deviation from the brief's wording: the brief described `/auto off` as escalating; it denied. Manual mode now
  asks, as the brief's mode table (and Claude Code's `default`) requires; `/auto off` and `--no-auto` mean manual.
- UI: `Effect::SetMode`, `StatusUpdate::Mode { mode, paused, denials }`; Shift+Tab sends the next mode in the
  cycle manual → accept edits → plan → auto → manual (a paused auto counts as auto); `/mode [name]` (spec table
  entry; `default|manual|accept-edits|acceptEdits|accept_edits|plan|auto`); `/auto` shows the mode, `/auto on|off`
  = auto/manual, `/auto resume` as before (from another mode it switches to auto). Status bar: `manual`,
  `accept edits`, `plan` (bold), `auto (deny c/3, t/20)`, `auto: PAUSED (…)` on yellow. Idle editor title
  ` ? keys · Shift+Tab mode `. `Modal::PlanReady(PlanDialog)`: when a turn ends in plan mode with a non-empty
  answer and nothing else is open, "1. Proceed (switch to accept edits and send "Proceed with the plan")
  2. Keep planning (Esc) 3. Proceed in manual mode"; ↑/↓/j/k + Enter, 1-3, Esc; held keys only move.
- Runner: `set_mode` switches the engine, pushes the status, saves `[auto] mode` in the realm `tui.toml`
  (`rules::persist_realm_mode`, drops the old `enabled` key, records the write in the host trust record like rule
  writes), and `note_plan_mode` appends `# Plan mode is on` / `# Plan mode is off` through
  `append_system_context` (keys `rkat-tui:plan-mode:on|off:<pid>-<nanos>-<n>`, unique per transition so a second
  entry into plan mode is not swallowed by idempotency; the brief said "fixed"). The runner remembers which
  session holds the "on" note; `start_turn` tells a session opened or resumed in plan mode before its first turn.
  Decision notices now also show human-decided calls of any tier (a manual-mode edit the user allowed).
- Layering: `[auto] mode` in global/realm/CLI layers (`mode` beats `enabled` in one layer, later layers win);
  `--set auto.mode=plan` works; a project `tui.toml` cannot pick a mode, its `enabled = false` only moves auto to
  manual (`enabled = true` is ignored now).
- Tests: `approval::mode::tests::{shift_tab_cycles_manual_accept_edits_plan_auto,
  names_parse_and_round_trip_through_toml}`; `approval::engine::tests::{manual_mode_still_asks_for_protected_writes,
  manual_mode_asks_for_tier_three_and_auto_resumes, accept_edits_allows_in_project_edits_that_manual_mode_asks_for
  (no classifier call, no dialog), accept_edits_asks_for_protected_writes_and_the_shell,
  plan_mode_denies_writes_and_commands_with_the_plan_error, a_helper_inherits_plan_mode,
  auto_mode_still_classifies_tier_three, a_queued_question_follows_a_mode_switch,
  the_mode_persists_in_the_realm_rules_file}`; `approval::rules::tests::the_permission_mode_layers`;
  `ui::commands::tests::mode_commands`; `ui::tests::{shift_tab_cycles_the_permission_mode, plan_ready_dialog,
  slash_commands_map_to_effects, status_updates_and_dirty_tracking}`;
  `ui::render::tests::{status_bar_modes_and_classifying, status_line_segments}`; render snapshots for the new
  idle title; end to end `integrator::plan_mode_refuses_a_write_and_the_model_reads_why` (SetMode(Plan) → the
  model's `write_file` comes back `is_error` with "plan mode is on", no file, no dialog, the system prompt holds
  `# Plan mode is on`, realm `tui.toml` holds `mode = "plan"`; switching to accept edits adds `# Plan mode is off`
  and saves `mode = "accept-edits"`).
- Docs: HELP_TEXT (Shift+Tab row, `/mode` and `/auto` rows), README "Permission modes" section (replaces the
  Shift+Tab-toggles-auto description; tier table and config reference rows), CHANGELOG.
- Commands: `cargo fmt --all` exit 0; `cargo clippy --all-targets --all-features -- -D warnings` exit 0;
  `cargo nextest run --all-features --no-fail-fast` 833 passed, 7 skipped, exit 0.

## Batch 3 B (2026-11-07)

Task checklist on Ctrl+T (`docs/ux-parity.md` row 22; candidate 7).

- Before: Ctrl+T was unbound (`Action::None`, kept for this); the engine's task tools (`task_create`, `task_get`,
  `task_list`, `task_update`, tier 1 in `approval/policy.rs`) ran but nothing showed their state.
- Source: the engine's own task store, no parsing of tool calls or results. With `session-store` the facade's
  dispatcher build gives every session `meerkat_tools::SqliteTaskStore::for_session(<store_path>/tasks.db,
  <session id>)` (`meerkat/src/factory.rs`, `build_tool_dispatcher_for_agent_with_overrides`), so the list is
  persistent and host-readable. `bootstrap::tasks_db(store_path)` mirrors that path (upstream ask: the facade
  names it only inside the dispatcher build) and `Runtime::tasks_db` carries it. `runner::checklist::read_checklist`
  lists the session's tasks through the same store type (no file: empty, and reading does not create one);
  the runner reads it when a session opens (start, `/new`, `/resume`, so a resumed session gets its list back)
  and after each `ToolExecutionCompleted` of `task_create`/`task_update` on the session's own event stream, and
  sends `Msg::Checklist`. A read error sends `Msg::ChecklistUnavailable`, which the next Ctrl+T names in one notice
  (the brief's resumed-session note only applies to a non-persistent source, which this is not). Helpers' tasks
  live in their own sessions and are not shown. `meerkat-tools` now names its `sqlite` feature explicitly
  (feature unification already enabled it); `chrono` parses the update stamps.
- UI: `ui::checklist::{Checklist, ChecklistItem, ItemState, ChecklistView}`: items ordered by `updated_at`
  (stable, the store's created order among ties), `☐`/`◐`/`☑`; `window()` shows up to 5 items, beyond that the
  4 updated last and `… and N more` above them (5 lines; the helper panel keeps its own 6, so both together are
  at most 11). Ctrl+T (`Action::ToggleChecklist`) toggles `checklist_view` and sends `Effect::SetChecklistView`;
  with no tasks nothing is drawn. `render::draw_checklist` draws under the input, before the helper panel;
  in progress bold yellow, done dim. Idle editor title: ` ? keys · Shift+Tab mode · Ctrl+T shows|hides the
  tasks ` while there are tasks.
- Config: `[ui] checklist = "hidden"|"shown"` (default hidden) layered like `[ui] helpers` (realm, project
  unless the command line set it, `--set ui.checklist=shown`); Ctrl+T writes it into the realm `tui.toml`
  (`rules::persist_realm_checklist` via `ApprovalEngine::persist_checklist`, recorded in the host trust record
  like the mode write).
- Tests: `ui::checklist::tests::{tasks_map_to_items_newest_updated_last, the_window_keeps_the_newest_and_counts_the_rest,
  the_view_toggles}`; `runner::checklist::tests::{the_checklist_is_the_sessions_tasks_in_the_store (another
  session's task is not listed), only_the_writing_task_tools_refresh}`;
  `approval::rules::tests::ui_checklist_defaults_to_hidden_layers_and_persists`; `ui::keymap` Ctrl+T;
  `ui::tests::{ctrl_t_toggles_the_checklist_and_records_the_view, an_unreadable_task_store_is_named_once_at_the_next_ctrl_t}`;
  render snapshots `ctrl_t_with_no_tasks_draws_nothing`, `the_checklist_renders_under_the_input` (three items,
  hint both ways), `more_than_five_tasks_show_the_newest_and_a_count`, `the_checklist_comes_before_the_helper_panel`;
  end to end `integrator::the_task_tools_fill_the_checklist_and_resume_restores_it` (the scripted model creates
  three tasks, then updates two by the ids the results returned (new `Step::Dynamic` in `tests/common`); the App
  fed by the runner shows ☐ run the tests / ☑ read the parser / ◐ write the fix after Ctrl+T; realm `tui.toml`
  holds `checklist = "shown"`; `--resume last` sends the same list before any turn; a new session's is empty).
- Docs: HELP_TEXT/keys panel Ctrl-T row; README "Task checklist" section, keys paragraph, config reference row;
  CHANGELOG.
- Commands: `cargo fmt --all` exit 0; `cargo clippy --all-targets --all-features -- -D warnings` exit 0;
  `cargo nextest run --all-features --no-fail-fast` 846 passed, 7 skipped, exit 0.

## Batch 3 C (2026-11-07)

Conversation rewind: `/rewind` (aliases `/checkpoint`, `/undo`) and Esc Esc on an empty idle input
(`docs/ux-parity.md` row 7; candidate 2). Code restore is out of scope.

- Before: Esc Esc on an empty input did nothing ("reserved for rewind"); no `/rewind`.
- Engine path (Step 1): the engine has two public ways to get "this session's history up to and excluding user
  message N", both on `SessionServiceTranscriptEditExt`, which `PersistentSessionService` (the TUI's
  `TuiService`) implements: (a) `rewrite_session_transcript` (same session id; the legacy
  `TranscriptRewriteSelection::MessageRange` with an empty replacement truncates; refuses a running session with
  `Busy`, converges the idle live session, keeps the old head as a retained revision that
  `restore_session_transcript_revision` brings back), and (b) `fork_session_at` (a new durable session with the
  prefix `Session::fork_at_complete_boundary` cuts, refusing a running source and a cut through a tool-use /
  tool-result pair; the source is untouched; the copied prefix keeps the system prompt and the metadata carries the
  model over, usage starts at zero). Chosen:
  (b). The same session id would keep the engine's per-session task store (`tasks.db`, scoped by session id)
  holding the tasks created after the cut, which the model's `task_list` would still see, and the TUI has no way
  to show or restore a retained transcript revision; a fork leaves the full conversation in a session `/resume`
  already lists, and switching to it reuses the `/resume` path (event follow, classifier transcript seeded from
  the fork, helper registration, checklist read, compaction trigger). No engine change and no engine ask.
  `inherited_instructions: ReactivateInherited`, so the fork never refuses over instruction activation rows.
- Runner (`runner/lifecycle.rs`): `Effect::RequestRewindMenu` reads the session and answers
  `Msg::ShowRewindMenu(rewind_points(..))`: every conversational user message with text, newest first, as typed
  (`ui::rewind::typed_text` drops the `@` mention attachments `expand_mentions` appended), leaving out the
  `(retrying the request above)` placeholder. `Effect::Rewind(point)` refuses while a turn runs, re-reads the
  session and refuses a point it no longer has ("the session changed since the rewind menu opened"), then
  `fork_session_at(message_index)`, `SessionHandle::resume`s the fork with the policy's compaction trigger, and
  `switch_to(.., reset = false)` (the previous session is retired, not archived), and sends
  `Msg::Rewound { from_end, text, blocks, notice }`. `from_end` counts the session's points from the chosen one to
  the end; `blocks` is the fork's history (`blocks_from_session`) for when the transcript does not line up.
- UI (`ui/rewind.rs`, `ui/mod.rs`, `ui/modal.rs`, `ui/transcript.rs`): `RewindMenu` (↑/↓ or k/j move, Enter
  chooses, Esc/q cancel; Ctrl-C closes) drawn as `Modal::Rewind`, one numbered one-line label per message and a
  key hint. `/rewind` and Esc Esc on an empty input (`escape`: a first Esc on an empty idle input now arms, the
  title says "Esc again to rewind") run `request_rewind`, which refuses with a notice while a turn runs, during a
  `/compact` rebuild, while a `!` command runs, or while anything is held (queued, send-next, a recall). An empty
  list is a notice. `Msg::Rewound`: `Transcript::cut_before_user(from_end, text)` truncates before the
  `from_end`-th top-level user block from the end when its typed text matches (`Transcript::truncate` keeps the
  tool index, helper folds, owners and open streams consistent), else the transcript is reset to the fork's
  blocks; the helper selection and full view close (the panel is derived from the kept blocks), the context gauge
  is cleared until the next call, the message's text goes into the editor, and the notice says the new session's
  id, that the previous session keeps the whole conversation (`/resume <id>`), and that files are not restored
  and the fork's task list starts empty. The checklist itself is the fork's, read by `switch_to`.
- Persisted: only what the engine's fork writes (the child session). Nothing in the TUI's own files.
- Tests: `ui::rewind::tests::{the_menu_moves_chooses_and_cancels, labels_are_one_line,
  typed_text_leaves_the_attachments_out}`; `ui::transcript::tests::a_rewind_cuts_before_the_chosen_user_block`;
  `ui::tests::{esc_esc_clears_the_draft_into_history (empty input now asks for the menu),
  rewind_asks_for_the_menu_only_when_idle_with_nothing_queued, the_rewind_menu_opens_chooses_and_cancels,
  rewound_cuts_the_transcript_and_refills_the_input}`; `ui::commands::tests` (`/rewind`, `/checkpoint`,
  `/undo`, usage error, spec count 31); render snapshot `rewind_menu_and_hint_snapshot`; end to end
  `integrator::rewind_forks_before_the_chosen_message_and_keeps_the_old_session` (three scripted turns; the menu
  lists three, two, one; rewinding to "two" switches to a new session id, `Rewound { from_end: 2, text: "two" }`
  with the fork's blocks "one"/"first answer"; the next turn's model request carries exactly the user messages
  "one" and the new prompt plus the persona; the old session still holds one/two/three and is listed; a stale
  point is refused).
- Docs: HELP_TEXT row for `/rewind` (from the spec table) and the keys panel's Esc Esc (empty input) row; editor
  title hint; README "Rewind" paragraph under Sessions, the command list and the idle-only list; CHANGELOG
  (also merged the second `### Fixed` heading under `[Unreleased]` into the original section).
- Commands: `cargo fmt --all` exit 0; `cargo clippy --all-targets --all-features -- -D warnings` exit 0;
  `cargo nextest run --all-features --no-fail-fast` 855 passed, 7 skipped, exit 0.

## Anthropic thinking per model (2026-11-07)

`[thinking]` in `tui.toml`: Anthropic thinking per model, carried in each session's provider params.

- Why: Anthropic binds replayed thinking blocks to the request prefix, so a request whose tool list changed (an
  MCP server connected, a resume) answers 400 unless it carries `thinking.block_binding.prefix_mismatch_behavior:
  "drop_block"`. meerkat-anthropic sends that control for catalogued ids; for an id outside the catalog (a proxy
  alias, a dated snapshot, a `[models.<id>]` profile) the upstream fix (lukacf/meerkat#2095, narrowed after
  review) sends it only when the request's Anthropic tag configures thinking explicitly (`Adaptive` or `Enabled
  { budget_tokens }`), since an unknown alias may be enabled-only. rkat-tui passed `provider_params: None`
  everywhere, so such a user had no way to get the control.
- Engine facts: the realm config refuses a realm-level `[agent] provider_params`
  (`meerkat-core` `reject_unwired_agent_provider_params`), so the host carries them per session:
  `SessionBuildOptions::provider_params` on create; `SessionLlmReconfigureRequest::provider_params` on a switch,
  a tri-state where `None` keeps the session's current params (`resolve_session_llm_identity_override`), so a
  switch that sent `None` would carry the previous model's thinking onto the next model; on resume the factory
  keeps the persisted params unless `ResumeOverrideMask::provider_params` is set
  (`resolve_resumed_session_llm_identity`). The factory already refuses, at build time, a thinking type a
  catalogued model does not accept (`validate_anthropic_request_shaping` over
  `ModelCapabilities::anthropic_provider_tag_rejection`).
- Shape: one table keyed by `endpoint:id` (wins) or bare id, values `"adaptive"`, `{ enabled = N }`, `"default"`
  (no override; a later layer undoes an earlier one). `"default"`, not `"off"`: `"off"` reads as the engine's
  `disabled` thinking type, which this setting does not send. Nothing is derived from a `[models.<id>]` profile's
  `supports_thinking` (it does not say adaptive vs enabled). User layers only, like `[pricing]` (a budget is
  billed); the project file's and an untrusted realm file's entries are ignored with a notice.
- Validation at start-up (`thinking::ThinkingTable::validate`): a reference to a non-Anthropic endpoint, a bare id
  the catalog gives another provider, and a type a catalogued Anthropic model refuses (the engine's own
  `anthropic_provider_tag_rejection` text) are dropped with a notice; an uncatalogued id passes through
  unchanged.
- Where it applies: `SessionHandle::create` (build params from `ModelChoice::thinking`); `switch_model` (always
  `Set` or `Clear`, never `None`); `SessionHandle::resume`, which a rewind (fork + resume) and every rebuild go
  through: the configured thinking for the persisted identity's endpoint and model is compared with the persisted
  tag's, and only on a difference does the build set the mask, replacing just the tag's `thinking`
  (`thinking::reapplied`; other persisted knobs stay). The classifier keeps `low_reasoning_params`. Helpers are
  left on the engine's default: `SpawnMemberSpec` has no fresh-spawn provider-params field (only the member
  profile's, which the engine's `delegate` builds, and `resume_provider_params`, which a fresh spawn rejects).
- Tests: `thinking::tests::{each_value_parses, the_settings_round_trip, a_bad_value_names_what_is_expected,
  a_reference_wins_over_the_bare_id_and_default_is_no_override, a_non_anthropic_endpoint_is_ignored_with_a_notice,
  a_catalogued_model_refusing_the_type_is_ignored_with_the_engines_reason, params_carry_only_the_thinking,
  a_resume_replaces_only_the_thinking}`;
  `approval::rules::tests::thinking_layers_replace_by_key_and_only_user_layers_count` (and the round trip);
  end to end `models::{configured_thinking_reaches_the_request_on_create_and_after_a_switch,
  a_resume_follows_the_thinking_configured_now}` (the scripted client's `LlmRequest` tag carries the configured
  thinking on create, after `/model`, cleared on a switch to an unconfigured model, picked up and dropped again on
  resume; with no setting the tag equals today's).
- Commands: `cargo fmt --all -- --check` exit 0; `cargo clippy --all-targets --all-features -- -D warnings` exit 0;
  `cargo nextest run --all-features` 873 passed, 7 skipped, exit 0.

## P1: the `rkat-tui-mob` daemon (2026-11-08)

A second binary that embeds the engine, holds the workspace realm, runs the
agent as one host-root mob member (mob `tui`, member `main`) and serves the
engine's own `MethodRouter` to any number of clients over loopback TCP
(`docs/mob-split.md` has the design; this is what the build relied on).

- Composition (`src/daemon/engine.rs`): `bootstrap::assemble` is the shared
  first half of `bootstrap_with` (endpoints, model, confinement plan, project
  trust, approval engine, MCP set, persistence opened, realm lease taken under
  a surface name, factory built); the TUI composes its `TuiService` on it, the
  daemon `meerkat_rpc::session_runtime::SessionRuntime::new_with_config_store`
  (crates/meerkat-rpc/src/session_runtime.rs:4373) with
  `require_hosting_mode`, `set_realm_context`, `set_config_runtime` and
  `set_default_llm_client` (the scripted client of the tests). Our own
  `MobMcpState` mirrors `compose_rpc_mob_state` (router.rs:173) without the
  connection callback-tool routes (`rpc_mob_external_tools_provider_from_parts`,
  `rpc_mob_slot_external_tools_provider`, `index_mob_callback_route_slot` are
  crate-private) and with `with_child_spawn_member_customizer`; it is set with
  `set_mob_state` before any `MethodRouter::new`, which then reuses it and
  installs its mob tools (router.rs:1724-1747).
- `meerkat-rpc` with `default-features = false, features = ["mob", "mcp",
  "comms"]` compiles: no `openai-realtime`, no `local-authorization` (the
  governed paths are cfg'd out). It still pulls axum and meerkat-live.
- The customizer (`src/mobs/customizer.rs`): `HelperCustomizer::with_root`
  names a `RootMember` (key `tui/main`) that is shaped as an owner (file and
  note tools as `external_tools`, the curated shell env, the TUI tool access
  policy, the persona and instruction files on a fresh spawn, the endpoint's
  auth binding) instead of a helper. `creation_origin` is `pub(crate)` in
  `SpawnMemberSpec` (meerkat-mob handle.rs:7357), so the root is recognised by
  mob and identity. `customize_resume` re-supplies the tools (verified: a
  restarted daemon's orchestrator calls `read_file`) and registers the
  member's durable session as a root owner in the `HelperRegistry`
  (`register_root`), so helpers it spawns or delegates to resolve to it. The
  root's `admission` is `None`: the P3 hook.
- The orchestrator (`src/daemon/orchestrator.rs`): the definition is
  `MobDefinition::implicit_with_provider` re-keyed to profile `main`, durable,
  `TurnDriven`, with builtins, shell, comms, mob, memory and the MCP servers
  on the profile (durable, unlike `external_tools`). `mob_list()` restores the
  persisted mobs first (`ensure_restored`); a missing member is spawned with
  `SpawnMemberSpec::host_root`; `mob_member_status` gives the session.
- Shutdown: `SessionRuntime::try_shutdown` stands the mob state down but
  "does not shut down the state's managed mobs" (session_runtime.rs, the
  `release_mob_state` doc). Without shutting them down, a second daemon in
  the same process failed to restore the mob: `MobQuarantined { ResumeRefused,
  ParticipantNameOccupied "tui/__mob_supervisor__" }`. The daemon now closes
  mob admission, shuts every mob in `mob_handles_snapshot` down, then shuts
  the runtime down (`stop_mobs`, the order `MobMcpState::stand_down`'s doc
  asks of a host).
- Transport: loopback TCP, decided over a Unix socket (versatility: the same
  loop as `rkat-rpc --tcp`, SSH port-forwarding, no platform differences).
  `serve_tcp_with_options_until` (server.rs:2309) binds its own listener, so it
  cannot report an ephemeral port or run a handshake; its structure is copied
  (`src/daemon/serve.rs`). Its connection/request admission, per-connection
  shutdown signal and shutdown hold are private, so shutdown is: stop
  accepting, remove the endpoint file, 2 s for connections to end, mobs and
  runtime shut down, remaining connections aborted. The engine's TCP path has
  no authentication (`--allow-remote` is "not an auth mechanism"), so each
  connection first sends `rkat/attach {token}` (`src/daemon/handshake.rs`);
  the token, address and pid are in `<workspace>/.rkat/tui/mob.endpoint`
  (0600, written by rename, removed on shutdown; a stale file is replaced when
  nothing accepts at its address).
- Not as the map says / worth knowing: `mob/turn_start` is request-scoped. It
  goes through `turn::start_turn_with_params` (handlers/mob.rs:2990), answers
  only when the turn has ended, and when its connection closes the server
  cancels the connection's requests at once (server.rs
  `hold_requests_for_shared_runtime_shutdown`: only a process shutdown holds
  them), which cancels the run at its next boundary: observed under load as
  `turn_completed` followed by `run_failed {class: cancelled, "Run was
  cancelled"}` 56 ms later. A client that may detach must use
  `mob/member_send {mob_id, agent_identity, content}` instead: it hands the
  input to the member and answers with a receipt at once, and the run is the
  member's. It needs the profile's `external_addressable = true` ("mob member
  is not externally addressable: main" otherwise), which the orchestrator's
  profile now sets. `events/list_since {scope: {type: "session",
  session_id}}` does work on a member session (the map had it untested): it
  replays `run_started` through `run_completed` after the sending client left.
- Limits: no approval gate on the orchestrator; helpers keep the helper gate
  but nothing serves an escalation and no user intent is recorded, and the
  gate has no tool origins (`ServiceToolOrigins` needs a `TuiService`), so
  their reviewed calls fail closed; no `fork` tool on the orchestrator (it
  needs the TUI's service); `[thinking]` provider params are not applied to
  the orchestrator; `RpcServer::new` resets the runtime's process-wide
  notification sink per connection (map risk (c)); running the TUI and the
  daemon on one workspace at once is unsupported (shared mob store).
- Tests: `tests/daemon.rs`: `a_client_attaches_and_finds_the_orchestrator`
  (endpoint file 0600, removed on shutdown), `a_connection_without_the_token_never_reaches_the_router`,
  `a_turn_streams_to_every_attached_client`,
  `a_client_leaving_mid_turn_leaves_the_turn_running` (`mob/member_send`,
  catch-up via `events/list_since`), `the_orchestrator_has_the_tui_file_tools`,
  `the_orchestrator_shell_runs_confined` (`SKIPPED:` without a sandbox),
  `a_restarted_daemon_restores_the_orchestrator_with_its_tools`,
  `a_second_daemon_on_a_live_workspace_is_refused`; unit tests in
  `daemon::handshake`, `daemon::endpoint`, `daemon::tests`,
  `mobs::customizer::tests::the_root_member_is_shaped_as_an_owner`,
  `mobs::registry::tests::a_host_root_member_owns_what_it_spawns`.

## P2: `rkat-tui --attach` (2026-11-08)

The TUI as a client of the `rkat-tui-mob` daemon (`src/client/`,
`docs/mob-split.md` Status). Engine facts this relied on, probed against the
P1 daemon with a scripted LLM before the client was written:

- Sequence numbers: a `mob/stream_event` notification has two. Its own
  `sequence` is a per-stream counter (1, 2, … from `mob/stream_open`); the
  carried envelope's `seq` is the session's durable sequence, the same
  number `events/list_since` reports as `cursor.sequence` for the same
  event, and it keeps counting across a daemon restart (a third turn after a
  restart: live `seq` 12..17, replay 12..17, stream `sequence` 1..6). The
  de-duplication rule rests on that: one high-water mark per session
  (`client::catchup::Mark`), an event is shown only when its envelope `seq`
  is above it; the live stream is opened before the replay is read, so
  nothing falls between them, and whichever delivers an event first shows
  it. The mark is saved in `<workspace>/.rkat/tui/attach.cursor` on every
  run terminal and on detach.
- The replay projection lags the live stream: right after `run_completed`
  arrived live, `events/list_since` returned only up to `text_complete`
  (4 of 6). A client attaching just after a turn ended can therefore find
  the tail in neither (published before its stream opened, not yet
  projected). The runner reads the replay again 500 ms after every attach
  and, while a turn runs, 500 ms after the last event
  (`CATCH_UP_EVERY`); the mark drops the overlap. Seen in
  `a_client_that_left_mid_turn_sees_the_turn_once_on_reattach` before the
  re-read existed (the reattached client stopped at `text_delta`).
- A run found in progress (its `run_started` already shown by the client
  that detached) is recognised by its first run-scoped event
  (`turn_started`, text, tool execution, `turn_completed`, run terminal)
  arriving while no turn is active: the UI gets `Msg::RemoteTurn { prompt:
  None }` and is busy until the run's end. A `run_started` while idle
  (another client's prompt) gets `RemoteTurn` with the prompt.
- `/model`: `mob/turn_start` with `model` needs `provider` for a model
  that is not the session's (`claude-opus-4-6` without one: -32602 "no
  capability profile is registered for provider 'anthropic' and model …";
  `claude-opus-4-8` with `provider: "anthropic"` works). The override
  persists: the next `mob/member_send` turn and a turn after a daemon
  restart ran on the new model. A prompt-less switch is not needed
  (`reconfigure_session_llm_identity` is not reachable over RPC for a
  member session; not probed further). Custom endpoints need an
  `auth_binding` override and are refused in attach mode for now.
- `mob/stop_member_run` takes `{mob_id, agent_identity, run_id, reason}`
  (`MobStopMemberRunParams`, `deny_unknown_fields`); the run id comes from
  the stream's `run_started` identity. On a scripted LLM call that never
  returns it ends the run with `run_failed`, which the client reports as
  interrupted. `mob/force_cancel` takes `{mob_id, agent_identity}`; it is
  sent on a second Esc after 1 s or when no run was reported yet, and an
  interrupt the daemon does not confirm within 10 s gives the UI back.
- `events/snapshot {scope}` gives the session's `model` and `provider` (the
  status bar and the context window on attach).
- `mob/member_status` gives `current_session_id`; the client re-reads it on
  every attach and starts from 0 when the cursor is for another session.

Design: the attach runner serves the UI's `Effect`/`Msg` contract itself
(`client::runner::AttachRunner`) instead of going under the in-process
runner through a session trait, because that runner is built on the
in-process `Runtime` (approval desk, helper host, `fork`, model lists, the
user shell's confinement plan). Events go to the UI as `Msg::Agent` on the
integrator channel so the turn messages stay ordered with them. A turn ends
300 ms after its last run ended with no new run (a steer that missed the
boundary runs as a follow-up run). Start-on-demand spawns `rkat-tui-mob`
with the shared engine flags (`client::launch::daemon_argv`, round-trip
tested through `DaemonCli`) in its own process group (`process_group(0)`;
`setsid` needs `pre_exec`, which is `unsafe`), and only on the first
connect; a reconnect waits for a live endpoint with backoff (100 ms to 2 s).
`/daemon stop` signals the endpoint file's pid only when the file's address
is the one this client is attached to. `rkat-tui-mob` refuses `--attach`.

Tests: `tests/attach.rs`: `a_prompt_is_answered_and_rendered`,
`esc_interrupts_a_hanging_turn`, `model_switches_with_the_next_prompt_and_stays`,
`a_client_that_left_mid_turn_sees_the_turn_once_on_reattach`,
`the_client_reconnects_to_a_restarted_daemon`,
`what_the_daemon_cannot_do_yet_says_so`; unit tests in `client::transport`
(handshake, refusals, pipelining, a call pending when the connection drops,
timeout, nothing listening), `client::launch` (decision for absent,
unparseable, live and stale files; the daemon argv round trip; a daemon that
exits), `client::catchup`, `client::runner` (model references, the stop
target, a turn's end), `cli::tests::attach_defaults_to_auto_and_no_attach_is_in_process`,
`ui::tests::a_remote_turn_shows_its_prompt_and_keeps_the_ui_busy_until_it_ends`,
`ui::render::tests::the_attach_state_leads_the_status_line`.

## P3: approvals on the daemon side (2026-11-08)

The daemon's orchestrator and its helpers run behind the TUI's approval
engine; consent is presented in the attached TUI and decided over the
engine's `approval/*` methods (`docs/mob-split.md` Status has the design).
Engine facts this relied on (read in the pinned checkout, then observed):

- `SessionRuntime::approval_service()` is `persistence.approval_issuer()`
  (session_runtime.rs:4232), the same `ApprovalService` the bootstrap hands
  the escalation ledger, so every escalation record the ledger writes is
  what `approval/list|get|decide` serve. The service is in-process state
  behind an `Arc` (clones share it); it has no notification for a plain
  decision (only `subscribe_consent_deliveries` for R3 consent), so the
  daemon's desk polls `get` every 100 ms and the client polls `approval/list`
  every 250 ms while a turn runs or a dialog is open.
- `approval/decide` takes `{approval_id, decision: approve|deny, actor,
  reason, provenance}` (`ApprovalDecideParams`); the record keeps `reason`
  and `provenance` in its `decision`. A second decision is refused
  `AlreadyDecided` (-32602 over the wire): the first client wins and a late
  one is a no-op. When the engine then records the answer it waited for,
  `EscalationLedger::settle` confirms a record already decided the same way
  instead of failing (it used to be a ledger failure, i.e. a denial).
- `approval/list` has only a status filter; the client filters requester
  (`rkat-tui-approval-gate`) and owner session itself. A helper's escalation
  is owned by the session it works for, so the orchestrator's client sees
  it, with the helper named (`RecordedPrompt.agent` in the request body).
- The published tool definitions (`published_live_visible_tool_defs`) are
  not reachable through the RPC runtime (`persistent_service` is
  `pub(crate)`); `MobSessionService::live_visible_tool_defs` is a session
  command that answers nothing while a turn runs, so the gate's catalog is
  read at start and after each run (`DaemonToolOrigins`), with the TUI's
  own tools known before that.
- `ToolConsequencePolicyRegistry`: a provider returns `Allow`, `Deny` or
  `Indeterminate`; `Indeterminate` refuses the call, so a narrow-only policy
  must allow what it does not deny. Evaluations run on supervisor worker
  threads with a 50 ms default deadline, and a missed deadline marks the
  provider mechanically unhealthy for good (every later call refused); the
  daemon sets 2 s. Installing the registry makes the host "managed": child
  mobs (`delegate`'s implicit mob, `mob_create`) then require
  `with_child_application_tool_policy`; host mobs keep the spawn's own
  binding, so the customizer sets it on the orchestrator and its same-mob
  helpers. The binding is persisted in the session metadata
  (`tooling.application_tool_policy`) and resolved at every build.
  Observed: a daemon started without the registry comes up (the member is
  rebuilt lazily) and the first `mob/member_send` fails with "member main
  failed to restore session …: a governed application tool policy requires
  the host policy registry".
- The orchestrator's member tools do not include `delegate` (that is the
  standalone session's tool); it spawns helpers with `spawn_member
  {profile, member_id, initial_message}`, which manual mode puts to the
  human like any Tier-3 call.
- `mob/turn_start` takes `provider_params` as `{"action": "set", "value":
  WireProviderParamsOverride}` or `{"action": "clear"}`; a resumed member
  takes new params through `SpawnMemberSpec::resume_provider_params`.
- Engine semantics worth knowing (unchanged from in-process): in manual mode
  an allow rule does not skip the question for an in-project edit (the
  edit's Tier 2 is decided before allow rules); "allow + add rule" stops
  repeat prompts for Tier-3 calls such as shell commands.
- Shutdown: a connection task aborted at shutdown must abort the proxy it
  spawned, or the client's socket stays open and it never reconnects (seen
  as `the_client_reconnects_to_a_restarted_daemon` hanging); the proxy's
  tasks are abort-on-drop.

Not built as briefed: `rkat/reload_rules` (the client writing `tui.toml`
and asking the daemon to reload). `/rules` is `rkat/rules`, run on the
daemon's engine, which already persists and trusts the realm rules file:
one writer, no reload of the trust checks. "Allow for this session" maps to
the in-process dialog's second choice (allow + add a literal rule): the
in-process gate has no session-scoped allow, and gated-tool grants
(`tool_grants/*`) do not apply to these tools.

Tests: `tests/attach_approvals.rs`: `a_tier_one_call_runs_without_a_prompt`,
`a_reviewed_call_shows_the_dialog_and_runs_after_approve`,
`deny_with_a_comment_refuses_and_the_comment_reaches_the_model`,
`allow_and_add_rule_skips_the_dialog_the_second_time`,
`an_unanswered_request_times_out_denied_and_the_ui_recovers`,
`two_clients_see_the_same_request_and_the_first_answer_wins`,
`auto_mode_needs_no_dialog_and_plan_mode_refuses_writes` (also: the
classifier read the user's words, the plan-mode note reached the model),
`a_deny_rule_added_in_attach_mode_refuses_the_next_call`,
`a_helpers_reviewed_call_raises_an_approval_too`,
`a_restarted_daemon_keeps_the_application_policy_binding`,
`the_local_methods_answer_before_the_engine`;
`tests/attach.rs::a_bang_command_runs_in_this_process_and_joins_the_agents_context`;
unit tests in `approval::remote` (answer ↔ record mapping, recorded prompt,
wire forms), `approval::ledger::tests::settling_confirms_a_record_a_client_already_decided_the_same_way`,
`daemon::approvals` (desk: decision handed back, interrupt stops only the
session's escalations, a dropped responder ends the wait), `daemon::policy`,
`daemon::local`, `client::approvals`. `tests/daemon.rs` now runs with an
allow-all classifier (its shell call is reviewed now).
