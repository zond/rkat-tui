# CLAUDE.md

Guidance for agents working in this repository.

## What this is

`rkat-tui` is an interactive terminal coding harness (a ratatui app) built on
the Meerkat agent engine. It is a standalone crate, not part of the Meerkat
monorepo: plain `cargo` is correct here, and none of the monorepo's wrappers,
lanes or environment variables apply. The engine is pulled in as the
crates.io release of Meerkat (`lukacf/meerkat`), one version for every
`meerkat-*` crate in `Cargo.toml`; see "Engine dependency" below.

Product direction: a focused coding application with Claude Code's keys and
feel, hosted on Meerkat's runtime-backed session path. Everything the engine
owns (session lifecycle, persistence, tool dispatch, mobs, confinement) is
used through its public API; the application owns the UI, the approval
policy, the classifier, prompts, tool selection and consent presentation. There are two binaries: `rkat-tui-mob`, the per-workspace daemon that hosts
the agent as a mob member and serves the engine's JSON-RPC over loopback
TCP (the proxy forwards only the engine methods the client uses,
`daemon::protocol::engine::SERVED`), and `rkat-tui`, the interactive TUI,
which is only its client and starts it on demand (`docs/mob-split.md`).
There is no in-process host: the in-process runner and `--no-attach` were
removed on 2026-11-10.

## Build and test

The gate, which CI runs verbatim on Rust 1.97.0:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --lib --bins -- -D warnings
cargo nextest run --all-features
RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" cargo doc --no-deps --all-features --document-private-items
```

Every step must exit 0 before a commit. Never commit on red, never loosen a
lint or delete a test to get there. The one exception: a commit that changes
only Markdown files outside `src/` and `tests/` (`README.md`, `CHANGELOG.md`,
`CLAUDE.md`, `docs/*.md`) skips the local gate, since no step reads them;
it still waits for its hosted CI run to end green. Anything else (code,
`Cargo.toml`, `Cargo.lock`, `deny.toml`, `.cargo/`, CI config, test
fixtures, a Markdown file a test or `include_str!` reads) runs the full gate. While iterating, scope the run
(`--lib`, `--test it -E 'test(/^<module>::/)'` for one file of
`tests/it/`, `-E 'test(<substring>)'`), then run the full gate
once. The first clippy run must always be `--all-targets --all-features`; the
second builds the library and binaries without `test-support`, so an
item nothing but the tests use is reported as dead. Put such items under
`#[cfg(any(test, feature = "test-support"))]` rather than making them
public. `cargo fmt --all` fixes formatting. `docs/testing.md` is the full protocol: where each kind
of test lives, the flaky-test history, running from inside the harness.

`.cargo/config.toml` sets the test-thread stack and the SQLite build flags
the engine needs; do not override `RUST_MIN_STACK` or `LIBSQLITE3_FLAGS`.

## Lints

The workspace lints are Meerkat's, copied verbatim: `pedantic`, `cargo` and
`all` at warn, `-D warnings` in the gate, and hard denies on `unwrap_used`,
`expect_used`, `panic`, `todo`, `unimplemented`, `dbg_macro`,
`redundant_clone`, `implicit_clone`; `unsafe_code` is denied (the one exception:
the two `unsafe` calls of SQLite's start-up switch in `src/main.rs` and
`src/bin/rkat-tui-mob.rs`, each with its SAFETY comment). Library code
propagates errors with typed enums (`thiserror`) and `?`; a test module
opens with `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`
when it needs them. The agent's shell is launched only through the engine's
confinement; the only direct `std::process::Command` launches in library
code are the user's own editor (`src/ui/editor.rs`), the user's `!`
shell (`src/user_shell.rs`), the client's start-on-demand of the
`rkat-tui-mob` daemon (`src/client/launch.rs`, through `setsid` when on
`PATH`, else in its own process group; output to the realm's log), and
the daemon's `ssh` for `/wire ssh://…` (`src/daemon/remote_wire.rs`
`Ssh::spawn`: an argv, never a shell, the host validated and after `--`,
`BatchMode=yes`; only a client's `rkat/wire_ssh` and a stored wire start
it, nothing the agent can call), and they stay the only ones.

## Layout

| Path | Owns |
|---|---|
| `src/main.rs`, `src/cli.rs` | Entry point and `clap` arguments |
| `src/bootstrap/` | The agents' personas (`persona.rs`); where the workspace's realm lives (`realm_home.rs`: manifests, lookup, re-home, the exposure check), the workspace's `tui.toml` layers and realm config (`read_layers`), what the daemon composes over (`assemble`: endpoints, model, the confinement plan `confinement.rs`, the approval engine, persistence, factory), project trust (`project_trust.rs`), the host trust record (`trust.rs`), the user's skills directory as a skill source and its `/context` report (`skills.rs`), the instruction files, the user's `instructions.files` among them (`instructions.rs`), the MCP servers of the files, the realm config and `--mcp-server` (`mcp.rs`) |
| `src/config/` | `tui.toml`: every scalar setting declared once (`registry.rs`: key, kind, default, the project rule, when a change applies, flags and commands; the layering, `--set` and the manual's table read it), the schema (`file.rs`), the layering of global, realm, command-line and project files (`layers.rs`), the rules, statements and directories the user's own layers give, at start and on every change of their files (`lasting.rs`), the one writer of the user's and the realm's file (`write.rs`, `SettingsFile`: one key in place, a directory lock) and the pair of them (`persist.rs`, `SettingsFiles`), the settings the UI renders and the daemon applies (`settings.rs`: compaction, the checklist view, helper headers), which models helpers run on (`helper_models.rs`: `helpers.model`, the allow or deny list, the refusals) |
| `src/approval/` | The approval engine: the decision path, what should happen then the Stop, attendance and the broker each in one place, and the escalation's phases (`engine.rs`), the user's permission changes and the lasting permissions held from the files, each change one with its file's write (`engine/permissions.rs`), policy tiers (`policy.rs`), rules and their patterns (`rules.rs`), classifier and its prompts (`classifier.rs`, `prompts.rs`), the review of a run at its checkpoint (`checkpoint.rs`), the classifier slot: its model, the checks, the probe, `/auto model` (`reviewer.rs`), the dispatch admission the daemon installs (`admission.rs`), permission modes (`mode.rs`), attendance and a session's runs (`attendance.rs`), escalation, the desk (`desk.rs`), the grants an approval in `/permissions` makes and the directories they join (`grants.rs`), the calls its Blocked section lists and the approvals once (`blocks.rs`), build trust after a dependency change (`builds.rs`), the kinds of action the user allows (`statements.rs`), the project's git hosts and the hosts the user trusts (`hosts.rs`), the agents' scratch directories (`scratch.rs`), the review numbers (`stats.rs`), the audit lines and the events' outlet (`audit.rs`), the permission corpus and its one evaluation (`corpus.rs`, cases in `corpus/*.toml`) |
| `src/daemon/`, `src/bin/rkat-tui-mob.rs` | The `rkat-tui-mob` daemon: composition over `bootstrap::assemble` with the agents' host (`engine.rs`), the handles the agents and the local methods share (`core.rs`), the workspace's mob and the agents' profile (`mob.rs`), the TCP serve loop and the proxy with its method allow-list (`serve.rs`), the `rkat/attach` token handshake (`handshake.rs`), the `rkat/*` local methods (`local/`: the dispatch table and the observer of forwarded requests in `mod.rs`, conversations and leaving `lifecycle.rs`, recall and input fates `inputs.rs`, the user's interrupt bound to its run `interrupt.rs`, `/btw`'s side question and its cancel `btw.rs`, the model, the classifier's model and the permission mode `model.rs`, wires `wires.rs`, `rkat/report` and the other reads `report.rs`), the wire types and codes (`protocol.rs`), the gate's desk and tool catalogs (`approvals.rs`), consent as durable decisions pushed to clients (`decisions.rs`), the members' tool policy (`policy.rs`), every setting's layers now and the one path a change takes (`settings.rs`, the hooks and the sync of a changed file in `local/settings.rs`, following the files in `settings_watch.rs`), the smart auto-compaction's prepare instruction and `compact_ready` (`prepare.rs`), `/btw`'s side call: its prompt, record budget and model call (`btw.rs`), the checkpoints of long runs at the engine's turn limit (`checkpoint.rs`: streaks, lines, the one under way; its steps for an agent and a helper in `agents/checkpoint.rs`), `assign`'s helpers, seated beside their caller, fresh or forked through the engine's own `fork_off`, single-use or kept (`assign.rs`), the tools an agent's requests leave out until it searches for them (`deferral/`: what stays when the engine's deferral ships and the members' mob tool surfaces a fork goes through, `ForkRoutes`, in `mod.rs`, the search and reveal state that goes then in `reveal.rs`), the live agents and their seating (`agents/`: the table and its seating rules in `mod.rs`, the names `names.rs`, what the agents are told of each other and how every note reaches an agent `notes.rs`, what the daemon last sent each agent `sent.rs`, runs no client started `triggers.rs`), when an agent is idle (`activity.rs`, pushed as `rkat/idle`), the checklist's tasks (`tasks.rs`), every model call's usage by conversation (`usage.rs`), the agents' schedules (`schedules.rs`) and file watches (`watches.rs`), conversations (`conversations.rs`), clients, owner and detach state (`lifecycle.rs`), the endpoint file (`endpoint.rs`), a full or failing disk under the realm (`storage.rs`: the plain words, the reload of a member the engine left needing one, the low-space warning), the agents' Unix sockets and wires to agents of other workspaces (`wires.rs`), wires to agents on other machines over ssh: the line protocol, the ssh argv and launch, the held links (`remote_wire.rs`) |
| `src/client/` | The TUI as the daemon's client: `--list` (`list.rs`), the runner (`runner/`: the loop and its reports in `mod.rs`, the connection's lifecycle as one state `conn.rs`, the connection `link.rs`, the turn `turn.rs` with its state and transitions in `turn/state.rs`, conversation, agent and workspace switches `switch.rs`, leaving `depart.rs`, the daemon commands `commands.rs`, `/btw` `btw.rs`), the connection (`transport.rs`), the daemon's notifications decoded once (`push.rs`), start-on-demand (`launch.rs`), `--attach` targets (`address.rs`), the member's methods (`remote.rs`), catch-up and de-duplication (`catchup.rs`), the approval dialogs over `approval/*` (`approvals.rs`), the helpers' follower view from mob-wide streams (`helpers.rs`), `/new`, `/resume` and the view-only sessions (`sessions.rs`), `/wire` across workspaces and its picker's removal of a wire (`wires.rs`; the remote side of `/wire ssh://…`, `rkat-tui --wire-remote`, in `wire_remote.rs`; the runner's side in `runner/wires.rs`), `/auto check` and `--classifier-check` (`check.rs`) |
| `src/workbench/` | What the client does on its own side of the agent: the `!` shell, `/remember` and `/memory`, `/export`, `/context`, prices and the cost record, model lists and picker, unsent messages, the app it starts and the terminal session (`app.rs`); `AgentContext` reaches the agent over the connection |
| `src/ui/` | Elm-style UI: `App::update(Msg) -> Vec<Effect>` (`mod.rs`), the turn, the delivery of typed messages and compaction as small states (`turn_state.rs`), split by concern (`turns.rs`, commands held for a turn's end `deferred.rs`, `keys.rs`, peer messages `peer.rs`, `mouse.rs`, `dispatch.rs`, `delivery.rs`, `escalations.rs`, `helpers.rs`, `shell.rs`, the `/btw` side question `btw.rs`), rendering (`render.rs`), the input editor, the command table (`commands.rs`), completion, the modals' state and keys (`modal.rs`) and their drawing (`modal/draw.rs`), the list pickers of `/schedules` and the like (`picker.rs`), the checklist; `ui/tests/` holds its unit tests split the same way |
| `src/ui/transcript/` | The transcript model: blocks, the event-to-block mapping, helper folds and what it knows of its helpers (`helpers.rs`), the laid-out line counts (`layout.rs`), notices (`notices.rs`), a stored conversation as blocks (`restore.rs`), a tool call as one plain line (`tool_line.rs`), a detached job's outcome (`outcome.rs`), runs of helper messages or of one tool's calls drawn as one folded line (`runs.rs`) |
| `src/tools/` | The file tools the agent gets (the shell is the engine's), their search budget and cancellation (`scan.rs`), the watch tools (`watch.rs`), the agents' `compact_ready` (`compact.rs`), `assign` (`assign.rs`, over the daemon's host), and `skill_read` over the engine's skill runtime (`skill.rs`) |
| `src/mobs/` | Helpers (`assign`: fresh or forked, single-use or kept) as mob members: the customizer with the members' tool policy, follower, registry, how each helper ends and who is told (`endings.rs`), the council mob a `council` call creates (`council.rs`), the `delegate` helpers a realm of an earlier build holds (`delegates.rs`; the engine's `delegate` is not offered) |
| `src/memory/` | Notes at user and workspace scope, memory tools |
| `src/endpoints/` | LLM endpoints: the types and `Endpoint`'s kinds (`mod.rs`: built-in, gateway, self-hosted), `endpoint:model` references (`refs.rs`), which endpoints this process has (`discover.rs`), their projection into the realm document (`project.rs`), the start-up plan (`startup.rs`) |
| `src/models.rs`, `src/pricing.rs`, `src/cost.rs`, `src/thinking.rs` | Model listing, price table, token totals by model and their price (the daemon counts them, `daemon/usage.rs`), the per-model `[thinking]` setting |
| `src/mentions.rs`, `src/paths.rs`, `src/overlay.rs`, `src/http.rs`, `src/hex.rs`, `src/text.rs`, `src/user_shell.rs`, `src/error.rs`, `src/late.rs` | `@` mentions and the path index, path expansion and the one atomic writer (`write_atomic`), `--set` overlays, the HTTP client and lenient fields, hex digests and random ids, counts, durations and the one set of text cuts (`one_line`, `head_chars`, `head_bytes`, `tail_bytes`) in messages, the user's `!` shell, the crate's error types, the handles the daemon's start-up binds once (`Late`: a warning the first time one is reached unbound) |
| `src/state_fixtures.rs` | The stored-state guard: opens the first release's files (`tests/fixtures/state-0.0.1/`) with this build's readers |
| `tests/` | Integration tests, one binary `it` (`tests/it/main.rs` declares one module per file of `tests/it/`), through the `test-support` feature on the one rig: the real daemon with a scripted LLM (`tests/it/common/rig.rs`: `Setup`, `Attached`, `Running`, `Client`); `tests/it/common/mod.rs` holds the scripted LLM, the workspace and the command lines |
| `docs/` | `testing.md` (protocol), `evidence.md` (what was built, why, with the probes and test names), `mob-split.md` (the architecture, its words, its status and the engine asks), `ux-parity.md` (Claude Code keys and behaviour compared), `state.md` (every stored file, its format and writer) |

The README is the front page; the full user manual is `docs/manual.md`.

## Conventions

- Every behaviour change ships with a test next to the thing it changes
  (`docs/testing.md` says which kind goes where) and a line under
  `## [Unreleased]` in `CHANGELOG.md` when it is user-visible.
- `docs/evidence.md` is the engineering record: a change that took a probe,
  a dead route or a design decision gets a short entry there (what was tried,
  what the engine does, the test names that pin it).
- The UI is Elm-style: state changes only in `App::update`, side effects are
  returned as `Effect`s and run by the client runner. Rendering tests draw into a
  ratatui `TestBackend` and assert on rows; there are no snapshot files.
- Commands are declared in one table (`src/ui/commands.rs`) and `/help` is
  generated from it; the key reference (`KEYS_HELP`) is hand-kept next to
  the keymap and changes with it.
- Paused-clock tokio tests count a task blocked in `spawn_blocking` as idle,
  so the virtual clock jumps; re-check deadlines after any await.
- Commit messages say what changed and why, in sentences. Commits made with
  an agent end with its `Co-Authored-By:` trailer.

## Engine dependency

`Cargo.toml` depends on every `meerkat-*` crate at one crates.io release,
currently 0.9.18; there are no git sources. Moving the version is the pin
move: change the version in every dependency line, `cargo update -p
meerkat`, `cargo check --all-targets --all-features`, adapt to API drift by
reading upstream's replacement in the fetched sources under
`~/.cargo/registry/src/`, run the gate, and record the move in the
changelog. A git revision comes back only for a fix the application needs
that is not released yet and has an open upstream PR; then every
`meerkat-*` line names that one revision until a release carries the fix.

Upstream policy: keep upstream work minimal. A gap with no application-side
workaround gets a small, contained PR to `lukacf/meerkat` written as a bug
report with the fix attached (reproducer, measurements, tests), from a branch
on the fork based on upstream `main`. Anything else is worked around here or
recorded as an ask in `docs/evidence.md`. Do not fabricate engine
capabilities the application does not have (for example mob membership for
the main session); keep a limitation explicit and typed instead.

## Upstream

Meerkat Support: https://support.rkat.ai, API key in
`~/.config/meerkat/support/rkat-tui-8e05e3.key` (never put a key in the
repository). Open upstream issues this code waits on:

- lukacf/meerkat#2180: an exact tool catalog for a mob member's
  dispatchers. rkat-tui's own search and reveal
  (`src/daemon/deferral/reveal.rs`) goes when it ships; `tests/it/tool_costs.rs`
  fails on purpose that day.
- #2184: updating a stored mob definition (the agents' override profile,
  `src/daemon/mob.rs` `main_profile`). 0.9.15 ships the update, but each
  member's roster row keeps the override profile it was seated with
  (still so in 0.9.18), so the override stays until a rebuild can drop
  it (`docs/evidence.md`, "Meerkat 0.9.15's fixes for our asks").
- #2126: rewinding a session in place under the same identity; `/rewind`
  returns then.
- #2206: a `delegate` call that times out leaves its helper seated.
  #2207: a deadline of `delegate`'s own. #2208: a `delegate` helper's mob
  discoverable after a failed call. #2224: a `delegate` call that times
  out returns an error with no helper identity. Their workarounds (the
  orphan retirement, `default_delegate_wait`, the persona's
  `implicit-<your session id>` line) are gone with `delegate` itself,
  which rkat-tui withdrew on 2026-11-16 (`assign` replaces it); the issues
  stay open asks, nothing of ours waits on them now.
- #2225: `member_not_found` from a per-member tool does not say that the
  member is in another mob the caller owns. Its workaround (the #2208 hint
  line) is gone with `delegate`; an open ask.

- #2231: a call that compacts an idle session. The quiet run `/compact`
  starts itself (`ui::COMPACT_PROMPT`) goes when it ships.
- #2232: a `fork_off` child (an `assign` fork) wired to its parent; #2233: an agent wiring its
  own children, and `auto_wire_parent` across mobs. The daemon wires each
  fork to its parent once it is seated, and holds the fork's calls until
  then (`src/mobs/follow.rs` `wire_to_parent`, `ApprovalGate::waiting_for`):
  both go when #2232 ships. When #2233 ships, `connect_helpers`
  (`src/daemon/connect.rs`, the host's batch wiring for an agent that may
  not wire the workspace's mob itself) can become the agent's own
  `wire_members`, and its refusal of helpers in different mobs goes if
  `auto_wire_parent` across mobs comes with it.
- #2374: a mob shutdown during a `fork_off` result's delivery leaves the
  owner's unregister unfinished (the stop then takes about 30 s). Nothing
  of ours goes when it ships; the tests with forks wait for the result's
  run to end before they stop the daemon because of it.
- #2384: a mob shutdown during a `fork_off` child's first run retires the
  child as failed (the parent reads "finished (failed)" after the restart).
  Nothing of ours goes when it ships; `forks::a_fork_stopped_in_its_first_run_is_retired_and_its_parent_told`
  pins the engine's behaviour and the manual's sentence on it changes then.
- #2234: the OpenAI dialect lowering nested tool schemas. The client
  wrapper `src/daemon/schema_compat.rs` goes when it ships.
- #2388 (filed 2026-11-17): the dialect clients replaying an assistant's
  empty or whitespace-only text (a proxy turns it into a placeholder
  sentence). Our filter `src/daemon/blank_text.rs` goes when it ships.
- #2560 (filed 2026-11-25): the OpenAI dialect clients replaying another
  provider's tool-call id unchanged (a long one is refused: `call_id` max
  64). Our filter `src/daemon/call_ids.rs` goes when it ships.
- #2579 (filed 2026-11-25): a host opt-in that keeps provider-native web
  search for a build with a tool dispatch admission (the engine strips
  every provider-native tool under our gate, so no agent searches).
  `agent.web_search = true` (`src/mobs/customizer.rs` `WebSearch`) sets
  the opt-in when it ships; until then it only leaves the engine's choice.
- #2432 (filed 2026-11-19): a host switch for the comms tools' usage
  section and the preloaded `mob-communication` skill, which teach
  `reply_to_peer`, `send_request`, `send_response` and `peers` to
  members that do not have them. Our filter `src/daemon/comms_text.rs`
  goes when it ships.
- #2411 (filed 2026-11-18): an empty completion fails a run nobody waits
  on (a peer's message, a daemon-started input), after three retries.
  Our recognition of it by the engine's wording and its quiet line
  (`src/ui/failures.rs`) goes or simplifies when it ships.
- #2235: a peer message that waits for an idle receiver's next run. The
  roster note's states and "do not message an idle agent with no client"
  (`src/daemon/agents/notes.rs`, the persona) go then.

- #2350: moving a tool call that is already running in the foreground to
  the background (Ctrl-B Ctrl-B). Background shell jobs themselves wait on
  #2180 (`approval::policy::SHELL_JOB_TOOLS`, `BACKGROUND_SHELL_UNAVAILABLE`
  go when a mob member's shell keeps its `Detached` contract).

- #2433 (filed 2026-11-20): engine events for facts we poll for (a
  spawned or forked member listed, wireable and registered; a session
  with no run and no pending input; a session actor live again after a
  reseat; six smaller ones). The polling loops it names
  (`src/mobs/follow.rs` `wire_to_parent`, `src/daemon/connect.rs`,
  `src/daemon/activity.rs`, the helpers' idle decision, the client's
  stream reopen) shrink to one
  await each as its parts ship.

- #2300: `blob_save_file` and `view_image` taking additional allowed
  directories from the host. The scratch-directory instruction's sentence
  about them (`src/mobs/customizer.rs`, the scratch instruction) goes when it ships.

## Stored state

`docs/state.md` lists every file rkat-tui stores (path, format, writer,
what losing it loses). From the first release on these formats are a
contract: state a tagged build wrote must keep opening; a change that
would break it needs the owner's explicit yes. The guard is
`src/state_fixtures.rs`: it opens the copies in
`tests/fixtures/state-0.0.1/` (taken from a run of the rig at the first
release) with this build's readers; never edit those copies to make it
pass.

## Sandbox facts worth knowing

The agent's shell runs under the engine's Linux confinement: an empty
private mount view with the workspace, the temp dir, the Cargo home (only
its `registry/` and `git/` writable, the rest read-only, its credentials
not granted) and a few system directories granted, its own pid namespace with a read-only
`/proc` of its own (best effort: a host that refuses the mount launches
without it), and a seccomp filter. Inside it the command is pid 2 under a
small init, host processes are invisible, and `/etc/alternatives` and `/usr/libexec` are granted so a
Rust toolchain can link. `--no-confinement` runs the shell as the trusted
host. The curated environment passes no provider keys into the sandbox.
The agent's shell is the daemon's; a `!` command is the user's own and
runs in the client as the user, with no sandbox, the user's shell and
whole environment (`src/user_shell.rs`); the client plans no confinement.
The daemon refuses an endpoint file its confined shell could read, and a
confinement plan that reaches the workspace's realm, which
lives outside the workspace in `<config dir>/workspaces/<id>/`
(`src/bootstrap/realm_home.rs`: a `workspace.toml` manifest per realm is
the only authority for which workspace owns it; re-homing is explicit).
Each agent's `/wire` socket lives there too (`<realm>/wire/<agent>.sock`,
0600 in a 0700 directory, `src/daemon/wires.rs`).
The confined shell can still write the workspace's `.rkat/` and `.git/`
(`.git/` only: an engine limit, recorded in `docs/evidence.md`). Tests keep their config directory,
and so the realms, beside the workspace (`common::config_dir_for`), never
in it and never in the user's `~/.rkat`.
