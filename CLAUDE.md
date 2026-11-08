# CLAUDE.md

Guidance for agents working in this repository.

## What this is

`rkat-tui` is an interactive terminal coding harness (a ratatui app) built on
the Meerkat agent engine. It is a standalone crate, not part of the Meerkat
monorepo: plain `cargo` is correct here, and none of the monorepo's wrappers,
lanes or environment variables apply. The engine is pulled in as git
dependencies on upstream `lukacf/meerkat`, pinned to one revision in
`Cargo.toml`; see "Engine dependency" below.

Product direction: a focused coding application with Claude Code's keys and
feel, hosted on Meerkat's runtime-backed session path. Everything the engine
owns (session lifecycle, persistence, tool dispatch, mobs, confinement) is
used through its public API; the application owns the UI, the approval
policy, the classifier, prompts, tool selection and consent presentation. There are two binaries: `rkat-tui`, the interactive TUI hosting the
agent in-process, and `rkat-tui-mob` (experimental), a daemon that hosts
it as a mob member and serves the engine's JSON-RPC over loopback TCP
(`docs/mob-split.md`).

## Build and test

The gate, which CI runs verbatim on Rust 1.97.0:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-features
```

Every step must exit 0 before a commit. Never commit on red, never loosen a
lint or delete a test to get there. While iterating, scope the run
(`--lib`, `--test <name>`, `-E 'test(<substring>)'`), then run the full gate
once. Clippy must always be `--all-targets --all-features`. `cargo fmt --all`
fixes formatting. `docs/testing.md` is the full protocol: where each kind
of test lives, the flaky-test history, running from inside the harness.

`.cargo/config.toml` sets the test-thread stack and the SQLite build flags
the engine needs; do not override `RUST_MIN_STACK` or `LIBSQLITE3_FLAGS`.

## Lints

The workspace lints are Meerkat's, copied verbatim: `pedantic`, `cargo` and
`all` at warn, `-D warnings` in the gate, and hard denies on `unwrap_used`,
`expect_used`, `panic`, `todo`, `unimplemented`, `dbg_macro`,
`redundant_clone`, `implicit_clone`; `unsafe_code` is denied. Library code
propagates errors with typed enums (`thiserror`) and `?`; a test module
opens with `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`
when it needs them. The agent's shell is launched only through the engine's
confinement; the only direct `std::process::Command` launches in library
code are the user's own editor (`src/ui/editor.rs`), the user's `!`
shell (`src/user_shell.rs`) and attach mode's start-on-demand of the
`rkat-tui-mob` daemon (`src/client/launch.rs`, through `setsid` when on
`PATH`, else in its own process group; output to the realm's log), and they stay the only ones.

## Layout

| Path | Owns |
|---|---|
| `src/main.rs`, `src/cli.rs` | Entry point and `clap` arguments |
| `src/bootstrap/` | Realm, config, factory and session-service construction; confinement plan (`confinement.rs`), project trust (`project_trust.rs`), instructions, MCP |
| `src/session/` | The session handle over the engine's service: turns, interrupt, tool access policy, resume (`resolve.rs` turns a session reference into an id) |
| `src/runner/` | The application loop between the UI and the session: effects, lifecycle (new, resume, rewind, compact), approval dialogs, shell, cost, quit |
| `src/approval/` | The approval engine: policy tiers (`policy.rs`), rules, classifier, the dispatch admission the session installs (`admission.rs`), permission modes (`mode.rs`), escalation, ledger, trust record |
| `src/ui/` | Elm-style UI: `App::update(Msg) -> Vec<Effect>`, rendering, input editor, command table (`commands.rs`), completion, modals, checklist |
| `src/tools/` | File tools and the shell tool the agent gets |
| `src/mobs/` | Helpers (delegate) as mob members: customizer, follower, registry |
| `src/daemon/`, `src/bin/rkat-tui-mob.rs` | The `rkat-tui-mob` daemon: engine composition over `bootstrap::assemble` (`engine.rs`), the orchestrator member (`orchestrator.rs`), the TCP serve loop (`serve.rs`), the `rkat/attach` token handshake (`handshake.rs`), the endpoint file (`endpoint.rs`), the `rkat/*` local methods (`local.rs`), the daemon-side approval gate and desk (`approvals.rs`), the members' application tool policy (`policy.rs`) and the orchestrator's conversations, `/new` and `/resume` (`conversations.rs`) |
| `src/fork/` | The `fork` tool: a child helper started with the parent's rendered conversation |
| `src/client/` | Attach mode (`--attach`, experimental): the daemon connection (`transport.rs`), start-on-demand (`launch.rs`), the member's methods (`remote.rs`), catch-up and de-duplication (`catchup.rs`), the approval dialogs over `approval/*` (`approvals.rs`), the helpers' follower view from mob-wide streams (`helpers.rs`), `/new`, `/resume` and `/rewind` (`sessions.rs`), the attach runner (`runner.rs`) |
| `src/memory/` | Notes at user and workspace scope, memory tools |
| `src/endpoints.rs`, `src/models.rs`, `src/pricing.rs`, `src/thinking.rs` | LLM endpoints, model listing, price table, the per-model `[thinking]` setting |
| `tests/` | Integration tests over the internal modules through the `test-support` feature; `tests/common/mod.rs` holds the scripted LLM and the rig |
| `docs/` | `testing.md` (protocol), `evidence.md` (what was built, why, with the probes and test names), `ux-parity.md` (Claude Code parity audit), `spec.yaml` / `checklist.yaml` / `traceability.md` (the RCT artifacts) |

The README is the front page; the full user manual is `docs/manual.md`.

## Conventions

- Every behaviour change ships with a test next to the thing it changes
  (`docs/testing.md` says which kind goes where) and a line under
  `## [Unreleased]` in `CHANGELOG.md` when it is user-visible.
- `docs/evidence.md` is the engineering record: a change that took a probe,
  a dead route or a design decision gets a short entry there (what was tried,
  what the engine does, the test names that pin it).
- The UI is Elm-style: state changes only in `App::update`, side effects are
  returned as `Effect`s and run by the runner. Rendering tests draw into a
  ratatui `TestBackend` and assert on rows; there are no snapshot files.
- Commands and keys are declared in one table each (`src/ui/commands.rs`,
  the keymap) and the help text is generated from them.
- Paused-clock tokio tests count a task blocked in `spawn_blocking` as idle,
  so the virtual clock jumps; re-check deadlines after any await.
- Commit messages say what changed and why, in sentences. Commits made with
  an agent end with its `Co-Authored-By:` trailer.

## Engine dependency

`Cargo.toml` pins every `meerkat-*` crate to one git revision of upstream
`lukacf/meerkat` `main`; there is no fork pin (the fixes the application
needed landed upstream, the changelog names them). Moving the pin: change
the revision in every dependency line, `cargo check --all-targets
--all-features`, adapt to API drift by reading upstream's replacement in the
fetched checkout under `~/.cargo/git/checkouts/`, run the gate, and record
the move in the changelog. The goal is to depend on a crates.io release
next.

Upstream policy: keep upstream work minimal. A gap with no application-side
workaround gets a small, contained PR to `lukacf/meerkat` written as a bug
report with the fix attached (reproducer, measurements, tests), from a branch
on the fork based on upstream `main`. Anything else is worked around here or
recorded as an ask in `docs/evidence.md`. Do not fabricate engine
capabilities the application does not have (for example mob membership for
the main session); keep a limitation explicit and typed instead.

## Sandbox facts worth knowing

The agent's shell runs under the engine's Linux confinement: an empty
private mount view with the workspace, the temp dir, the Cargo home and a
few system directories granted, its own pid namespace with a read-only
`/proc` of its own (best effort: a host that refuses the mount launches
without it), and a seccomp filter. Inside it the command is pid 2 under a
small init, host processes are invisible, and `/etc/alternatives` and `/usr/libexec` are granted so a
Rust toolchain can link. `--no-confinement` runs the shell as the trusted
host. The curated environment passes no provider keys into the sandbox.
