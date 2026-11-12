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
(`--lib`, `--test <name>`, `-E 'test(<substring>)'`), then run the full gate
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
shell (`src/user_shell.rs`) and the client's start-on-demand of the
`rkat-tui-mob` daemon (`src/client/launch.rs`, through `setsid` when on
`PATH`, else in its own process group; output to the realm's log), and they stay the only ones.

## Layout

| Path | Owns |
|---|---|
| `src/main.rs`, `src/cli.rs` | Entry point and `clap` arguments |
| `src/bootstrap/` | The agents' personas (`persona.rs`); where the workspace's realm lives (`realm_home.rs`: manifests, lookup, re-home, the exposure check), the workspace's `tui.toml` layers and realm config (`read_layers`), what the daemon composes over (`assemble`: endpoints, model, the confinement plan `confinement.rs`, the approval engine, persistence, factory), project trust (`project_trust.rs`), the host trust record (`trust.rs`), instructions, MCP |
| `src/config/` | `tui.toml`: the schema (`file.rs`), the layering of global, realm, command-line and project files (`layers.rs`), the realm file's trusted writes (`persist.rs`, `RealmFile`), the settings the UI renders and the daemon applies (`settings.rs`: compaction, the checklist view, helper headers) |
| `src/approval/` | The approval engine: policy tiers (`policy.rs`), rules and their patterns (`rules.rs`), classifier, the dispatch admission the daemon installs (`admission.rs`), permission modes (`mode.rs`), escalation, the desk (`desk.rs`) |
| `src/daemon/`, `src/bin/rkat-tui-mob.rs` | The `rkat-tui-mob` daemon: composition over `bootstrap::assemble` with the agents' host (`engine.rs`), the workspace's mob and the agents' profile (`mob.rs`), the TCP serve loop and the proxy with its method allow-list (`serve.rs`), the `rkat/attach` token handshake (`handshake.rs`), the `rkat/*` local methods including `rkat/report` (`local.rs`), the wire types and codes (`protocol.rs`), the gate's desk and tool catalogs (`approvals.rs`), consent as durable decisions pushed to clients (`decisions.rs`), the members' tool policy (`policy.rs`), the tools an agent's requests leave out until it searches for them (`deferral/`: what stays when the engine's deferral ships in `mod.rs`, the search and reveal state that goes then in `reveal.rs`), the live agents and their seating (`agents.rs`), when an agent is idle (`activity.rs`, pushed as `rkat/idle`), the checklist's tasks (`tasks.rs`), every model call's usage by conversation (`usage.rs`), the agents' schedules (`schedules.rs`) and file watches (`watches.rs`), conversations (`conversations.rs`), clients, owner and detach state (`lifecycle.rs`), the endpoint file (`endpoint.rs`) |
| `src/client/` | The TUI as the daemon's client: `--list` (`list.rs`), the runner (`runner/`: the loop and its reports in `mod.rs`, the connection's lifecycle as one state `conn.rs`, the connection `link.rs`, the turn `turn.rs`, conversation, agent and workspace switches `switch.rs`, leaving `depart.rs`, the daemon commands `commands.rs`), the connection (`transport.rs`), the daemon's notifications decoded once (`push.rs`), start-on-demand (`launch.rs`), `--attach` targets (`address.rs`), the member's methods (`remote.rs`), catch-up and de-duplication (`catchup.rs`), the approval dialogs over `approval/*` (`approvals.rs`), the helpers' follower view from mob-wide streams (`helpers.rs`), `/new`, `/resume` and the view-only sessions (`sessions.rs`) |
| `src/workbench/` | What the client does on its own side of the agent: the `!` shell, `/remember` and `/memory`, `/export`, `/context`, prices and the cost record, model lists and picker, unsent messages, the app it starts and the terminal session (`app.rs`); `AgentContext` reaches the agent over the connection |
| `src/ui/` | Elm-style UI: `App::update(Msg) -> Vec<Effect>` (`mod.rs`), split by concern (`turns.rs`, `keys.rs`, peer messages `peer.rs`, `mouse.rs`, `dispatch.rs`, `delivery.rs`, `escalations.rs`, `helpers.rs`, `shell.rs`), rendering (`render.rs`), the input editor, the command table (`commands.rs`), completion, modals, the list pickers of `/schedules` and the like (`picker.rs`), the checklist; `ui/tests/` holds its unit tests split the same way |
| `src/ui/transcript/` | The transcript model: blocks, the event-to-block mapping, helper folds (`helpers.rs`), notices (`notices.rs`), a stored conversation as blocks (`restore.rs`) |
| `src/tools/` | The file tools the agent gets (the shell is the engine's), their search budget and cancellation (`scan.rs`), and the watch tools (`watch.rs`) |
| `src/mobs/` | Helpers (delegate) as mob members: the customizer with the members' tool policy, follower, registry, the councils the gate records (`council.rs`), the helpers of `delegate` calls that ended without their result (`delegates.rs`) |
| `src/memory/` | Notes at user and workspace scope, memory tools |
| `src/endpoints/` | LLM endpoints: the types and `Endpoint`'s kinds (`mod.rs`: built-in, gateway, self-hosted), `endpoint:model` references (`refs.rs`), which endpoints this process has (`discover.rs`), their projection into the realm document (`project.rs`), the start-up plan (`startup.rs`) |
| `src/models.rs`, `src/pricing.rs`, `src/cost.rs`, `src/thinking.rs` | Model listing, price table, token totals by model and their price (the daemon counts them, `daemon/usage.rs`), the per-model `[thinking]` setting |
| `src/mentions.rs`, `src/paths.rs`, `src/overlay.rs`, `src/http.rs`, `src/hex.rs`, `src/text.rs`, `src/user_shell.rs`, `src/error.rs` | `@` mentions and the path index, path expansion and the one atomic writer (`write_atomic`), `--set` overlays, the HTTP client and lenient fields, hex digests and random ids, counts and durations in messages, the user's `!` shell, the crate's error types |
| `src/state_fixtures.rs` | The stored-state guard: opens the first release's files (`tests/fixtures/state-0.0.1/`) with this build's readers |
| `tests/` | Integration tests through the `test-support` feature on the one rig: the real daemon with a scripted LLM (`tests/common/rig.rs`: `Setup`, `Attached`, `Running`, `Client`); `tests/common/mod.rs` holds the scripted LLM, the workspace and the command lines |
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
currently 0.9.13; there are no git sources. Moving the version is the pin
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
  (`src/daemon/deferral/reveal.rs`) goes when it ships; `tests/tool_costs.rs`
  fails on purpose that day.
- #2182: an atomic "cancel if still queued" for `rkat/recall`
  (`Local::recall`'s lookup-then-cancel leaves a window in which the
  cancel interrupts a run that just started).
- #2183: a council's convener named by the engine (`src/mobs/council.rs`).
- #2184: updating a stored mob definition (the agents' override profile,
  `src/daemon/mob.rs` `main_profile`).
- #2126: rewinding a session in place under the same identity; `/rewind`
  returns then.
- #2206: a `delegate` call that times out leaves its helper seated.
  rkat-tui's orphan retirement (`src/mobs/delegates.rs`) goes when the
  engine retires the helper itself.
- #2207: a deadline of `delegate`'s own. The load-time default
  `default_delegate_wait` (`src/bootstrap/realm_config.rs`) goes when it
  ships.
- #2208: a `delegate` helper's mob discoverable after a failed call. The
  persona's hint line naming `implicit-<your session id>`
  (`src/daemon/deferral/mod.rs`) goes when it ships.

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
private mount view with the workspace, the temp dir, the Cargo home and a
few system directories granted, its own pid namespace with a read-only
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
The confined shell can still write the workspace's `.rkat/` and `.git/`
(`.git/` only: an engine limit, recorded in `docs/evidence.md`). Tests keep their config directory,
and so the realms, beside the workspace (`common::config_dir_for`), never
in it and never in the user's `~/.rkat`.
